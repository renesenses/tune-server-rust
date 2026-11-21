# TESS : le contrat (lot T01)

*Tune Engine for Sound Shaping.* Ce document décrit le module
`tune-core/src/tess/`. Il ne contient que des **types et des fonctions pures** :
rien n'est branché dans la lecture. Les lots suivants s'appuient dessus :
- T02 : modèle de chaîne et migration ;
- T03 et T04 : l'exécuteur ;
- T05 : le SDK en F64 ;
- T07 : les étages fixes ;
- T08 : l'API ;
- T12 : les zones réseau ;
- T16 : le chemin du signal ;
- W1 à W5 : l'écran.

Versions :
- version du contrat : **1** (`tess::VERSION_CONTRAT`) ;
- version du format de chaîne enregistrée : **1** (`tess::VERSION_CHAINE`).

## 0. Compatibilité (règle générale)

- **Un client ignore les champs qu'il ne connaît pas**, et lit toute valeur
  d'énumération inconnue comme `unknown`.
  - Les types Rust de réponse le font déjà : pas de `deny_unknown_fields`, et
    une variante de repli `#[serde(other)]` sur chaque énumération.
  - `tess::schema_json_lecture()` est le schéma qu'un client applique : champs,
    valeurs, variantes et bornes ouverts.
- **Le serveur, lui, est exact.** `tess::schema_json()` décrit précisément ce
  qu'il émet et accepte ; les tests le vérifient.
- `contract_version` ne monte que quand un champ change de **sens**. Un ajout
  facultatif ne la fait pas monter.
- **Le corps d'un `PUT` est lu strictement** (`ChaineTess::depuis_corps`). Un
  JSON illisible, un champ inconnu ou une valeur inconnue donnent un
  **400 `invalid_body`**.
- **Version de chaîne :**
  - un `PUT` est accepté pour toute version de 1 à `VERSION_CHAINE`, puis
    remonté au format courant (`mettre_a_niveau`) ;
  - une version plus récente que celle du serveur donne un **409
    `unsupported_version`** explicite ;
  - un serveur qui **lit** une chaîne enregistrée par une version plus récente
    (retour en arrière) ne l'applique pas à moitié : toutes les étapes de
    traitement s'éteignent avec le motif `chain_version_unsupported` et la zone
    redevient bit-perfect.

## 1. Étage de traitement (`tess::etage`)

```rust
pub trait EtageTess: Send {
    fn id(&self) -> &str;
    fn preparer(&mut self, entree: FormatFlux, max_trames: usize) -> Result<Disposition, ErreurEtage>; // hors temps réel
    fn traiter_sur_place(&mut self, tampon: &mut [f64], trames: usize) -> Result<(), ErreurEtage>;     // temps réel
    fn traiter_vers(&mut self, entree: &[f64], trames: usize, sortie: &mut [f64]) -> Result<usize, ErreurEtage>; // temps réel
    fn vidanger(&mut self, sortie: &mut [f64]) -> Result<usize, ErreurEtage>; // temps réel, défaut : 0
    fn reinitialiser(&mut self) {}
    fn bypass(&self) -> bool;
    fn definir_bypass(&mut self, bypass: bool);
    fn appliquer_reglages(&mut self, reglages: &Value) -> Result<(), ErreurEtage>; // hors temps réel
}
```

**Règles temps réel**

1. `preparer` est **hors temps réel**. L'étage y reçoit le format d'entrée et
   la **taille de bloc maximale**, et y alloue tout ce dont il a besoin. Il rend
   sa `Disposition` :
   - le format de sortie ;
   - le nombre maximal de trames en sortie (×N pour un suréchantillonneur) ;
   - sa latence, à la fréquence de sortie ;
   - s'il travaille sur place ou non.
2. **L'hôte préalloue** les tampons d'après les dispositions
   (`preparer_chaine`, qui propage format et taille de bloc d'étage en étage et
   additionne les latences).
   - Un étage qui garde le format et le nombre de trames travaille **sur place**.
   - Un étage qui change le nombre de canaux (réaffectation) ou la cadence
     (suréchantillonnage) écrit dans un **tampon de sortie distinct**
     (`traiter_vers`), préalloué par l'hôte.
   - Un étage « sur place » qui change de format est refusé à la préparation.
3. `traiter_*`, `vidanger` et `reinitialiser` **n'allouent jamais** et ne
   bloquent jamais.
   - `ErreurEtage` est `Copy`, sans `String`.
   - Un tampon trop petit donne `TamponTropPetit`, pas une panique.
4. Échantillons **entrelacés en `f64`**, pleine échelle ±1,0.
   - Aucune conversion `f32` ni entière entre deux étages (Q5).
   - La seule quantification est l'`EtapeFinale`.

**Comportements fixés**

- **Vidange** : en fin de flux, un étage à latence (fin de convolution,
  anticipation du Declick) rend sa queue par `vidanger`.
- **Contournement en cours de lecture** : l'étage rend son entrée intacte,
  **retardée de sa latence**. Cocher ou décocher un étage ne décale ni le flux
  ni la synchronisation multi-points.
  - « Bypass all » et le mode Pure reconstruisent la chaîne **sans** ces
    étages à la relance suivante : aucun retard, bit-perfect.
- **Réglages à chaud** : `appliquer_reglages` est hors temps réel.
  - L'hôte l'appelle sur une instance qui ne joue pas, ou construit une
    nouvelle instance, la prépare, puis l'échange entre deux blocs. Le fondu
    éventuel est décidé par T03.
  - Un étage qui ne sait pas changer à chaud rend `ReconstructionRequise`
    (comportement par défaut).

**Étape finale (`EtapeFinale`)** : quantification PCM (dither TPDF, mise en
forme du bruit ou aucun dither), ou modulateur sigma-delta vers DSD.
- Ce n'est **pas** un `EtageTess`, puisque sa sortie n'est pas en `f64` :
  `SortieFinale::Pcm` (entiers cadrés sur 32 bits, `bits` significatifs) ou
  `SortieFinale::Dsd` (1 bit, 8 échantillons par octet, bit de poids fort en
  premier).
- Mêmes règles temps réel que les étages.

## 2. Descripteur de greffon (`tess::greffon`)

Un greffon TESS est un traitement du SDK natif : référence compilée
(`origin: "reference"`) ou paquet `.tuneplugin` signé (`origin: "package"`).

| Champ | Sens |
|---|---|
| `id` | stable ; minuscules ASCII, chiffres, tirets ; ni tiret en tête ni en queue ; 64 caractères au plus (même règle pour les préréglages) |
| `name` | nom d'affichage anglais, traduit par les clients |
| `category` | groupe de la fenêtre « Add plugin » (A3) : `equalisation`, `spatial`, `room_and_filters`, `restoration_and_routing` |
| `entitlement` | `free` ou `premium` (inconnu = traité comme Premium) |
| `feature` | code de la `Feature` de `license.rs` qui ouvre le greffon, ou `null` (droit Premium général) |
| `origin` | `reference` ou `package` |
| `settings_schema` | schéma JSON des réglages (caisse du SDK ou manifeste du paquet) |
| `placement.channels` | `any`, `stereo_only` (seulement s'il le déclare), `remaps` (change le nombre de canaux) |
| `placement.position` | `free` ou `last_plugin` ; `remaps` impose `last_plugin` |
| `placement.exclusive_group` | `{ "id", "priority" }` ou `null` |

L'hôte vérifie les réglages d'un greffon à trois niveaux : objet ou `null` ;
clés `required` présentes ; aucune clé inconnue si `additionalProperties: false`.
Le reste est vérifié par le greffon.

### Groupe exclusif crossfeed / Crossfeed Pro / Crosstalk

Deux crossfeeds en série croisent les voies deux fois. Le crosstalk (pour
enceintes) agit en sens contraire du crossfeed (pour casque).

- **Politique `refuse` (par défaut, décision B de Bertrand du 18/11)** : une
  chaîne ne coche qu'un membre du groupe. Un `PUT` qui en coche deux est
  refusé en **409 `mutually_exclusive`**.
- **Résolution toujours tolérante** (règle de #7616), quelle que soit la
  politique : une chaîne ancienne qui arriverait quand même avec deux membres
  cochés n'est jamais réduite au silence.
  - À l'exécution, le membre actif le plus prioritaire traite.
  - Les autres sont écartés, avec le motif `replaced_in_group` et
    `params.replaced_by`.
  - Priorités : `crosstalk` (3) > crossfeed tiers `crossfeed-*` (2) >
    `crossfeed` intégré (1). À égalité, le premier dans la chaîne l'emporte.
- **Politique `refuse`** : décidée le 18/11 pour le groupe crossfeed. L'API
  (T08, `catalogue_tess`) l'applique : un `PUT` qui coche deux membres est
  refusé en 409 `mutually_exclusive`. La résolution garde la priorité
  ci-dessus pour une chaîne ancienne.
- **Politique `priority`** : reste disponible pour un groupe qui la déclare
  (jamais de refus, la résolution tranche).
- **Zones existantes (T02)** : la migration ne refuse jamais une zone où
  plusieurs membres sont cochés. Elle garde celui qui traite aujourd'hui et
  DÉCOCHE les autres, réglages gardés, marqués `"replaced_in_group": "<id du
  gagnant>"` ; la résolution l'affiche (`replaced_in_group`). Voir § 8.
- **Appartenance au groupe** : par le manifeste (`exclusive_group`) **et** par
  le préfixe `crossfeed-` pour les paquets déjà publiés. T05 ajoute le champ
  au manifeste.

### Catalogue de référence (v1.0)

| id | Nom | Catégorie | Droit | `feature` | Origine | Canaux | Place | Groupe (priorité) |
|---|---|---|---|---|---|---|---|---|
| `equalizer-graphic` | Graphic EQ | equalisation | Free | — | référence | any | libre | — |
| `equalizer-parametric` | Parametric EQ | equalisation | **Premium** (Q3) | à fixer (T19) | référence | any | libre | — |
| `equalizer-headphone` | Headphone EQ | equalisation | Free | — | référence | any | libre | — |
| `crossfeed` | Crossfeed | spatial | **Free** (Q2 révisée) | — | référence | stereo_only | libre | crossfeed_crosstalk (1) |
| `crossfeed-pro` | Crossfeed Pro | spatial | Premium | — | paquet | stereo_only | libre | crossfeed_crosstalk (2) |
| `crosstalk` | Crosstalk | spatial | Premium | — | paquet | stereo_only | libre | crossfeed_crosstalk (3) |
| `convolution` | Convolution | room_and_filters | Premium | `room_correction` | référence | any | libre | — |
| `room-correction` | Room correction | room_and_filters | Premium | `room_correction` | référence | any | libre | — |
| `declick-live` | Declick | restoration_and_routing | **Premium** (P1) | à fixer (T19) | référence | any | libre | — |
| `mono-downmix` | Mono downmix | restoration_and_routing | Free | — | référence | stereo_only | libre | — |
| `channel-remap` | Channel remapping | restoration_and_routing | Free | — | référence | remaps | dernier | — |

Remarques sur le catalogue :
- **Niveaux périmés dans la maquette A3** : Parametric EQ (« TIER TBD ») et
  Declick (« FREE ») y sont faux. Ce tableau fait foi.
- **Crossfeed en Free** : le passage dans `license.rs` et `premium_plugins.rs`
  n'est pas fait ici (T19).
- **Moteur partagé** : les trois égaliseurs sont trois façades d'**un seul**
  moteur (T06).
- **Declick** : `declick-live` est distinct de l'outil de fichiers `declick`
  (Q4).
- **Upsampler** : ce n'est pas un greffon mais un étage fixe.
- **Greffons privés** : Crossfeed Pro et Crosstalk viennent de dépôts privés.
  Le contrat n'en connaît que l'id, le droit et la place.
- **Schémas de réglages** : ceux du catalogue de référence sont minimaux
  (`{"type":"object"}`). T08 y branche ceux des caisses du SDK.

## 3. Chaîne d'une zone (`tess::chaine`)

Enregistrée sous `zone_{id}_tess_chain` (T02), échangée par
`PUT /zones/{id}/tess`.

```json
{
  "version": 1,
  "bypass_all": false,
  "plugins": [
    { "id": "convolution", "enabled": true, "settings": { "filter_set": "living-room.zip" } },
    { "id": "equalizer-graphic", "enabled": true, "settings": null },
    { "id": "crossfeed", "enabled": false, "settings": null }
  ],
  "fixed": {
    "headroom": { "mode": "auto" },
    "upsampling": { "mode": "pcm", "filter": "polyvalent", "max_ratio": 4, "max_rate_hz": null },
    "quantization": { "dither": "tpdf" }
  }
}
```

- **Champs obligatoires** : seul `version` l'est. Les autres ont une valeur
  par défaut : chaîne vide, headroom auto, upsampling éteint, TPDF.
- **`headroom`** : `{"mode":"auto"}` ou `{"mode":"manual","db":x}`, x dans
  [−24 ; 0].
  - Auto = −3 dB dès qu'un greffon ou l'upsampler est **effectif**, 0 dB sinon.
  - L'étage porte aussi le ReplayGain et le gain de zone, réglés comme
    aujourd'hui.
- **`upsampling`** (Premium) reprend **tous** les réglages d'`UpsamplerSettings`
  (#7614) :
  - `{"mode":"off"}` ;
  - `{"mode":"pcm","filter","max_ratio","max_rate_hz"}` :
    - `filter` : un des six filtres du SDK, `polyvalent` par défaut ;
    - `max_ratio` : 2, 4, 8 ou 16, ou `null` = **maximum du périphérique** ;
    - `max_rate_hz` : plafond de 1 à 768 000 Hz, ou `null` ;
  - `{"mode":"dsd","rate":"dsd64"|"dsd128"|"dsd256","filter"}`.
- **`quantization.dither`** : `tpdf`, `noise_shaped` ou `off` (sortie en
  entiers 32 bits, par exemple).
- **Volume** : il n'est pas dans la chaîne. Il reste un réglage de zone ; le
  contrat fixe sa place et son mode (§ 4).
- **`bypass_all`** : greffons, headroom et upsampling contournés. Restent le
  volume logiciel et la quantification qu'il exige.

### Validation d'un `PUT` (`ChaineTess::valider`)

| Code | HTTP | Règle |
|---|---|---|
| `invalid_body` | 400 | JSON illisible, champ inconnu ou valeur inconnue (`depuis_corps`) |
| `unsupported_version` | 409 | `version` hors de 1 à `VERSION_CHAINE` |
| `unknown_value` | 409 | réglage fixe lu `unknown` (chaîne relue d'une version plus récente) |
| `headroom_out_of_range` | 409 | headroom manuel hors [−24 ; 0] dB ou non fini |
| `invalid_upsampling` | 409 | filtre inconnu, `max_ratio` hors {2, 4, 8, 16}, `max_rate_hz` hors bornes |
| `unknown_plugin` | 409 | id absent du catalogue |
| `duplicate_plugin` | 409 | même id deux fois |
| `invalid_settings` | 409 | réglages non conformes au schéma (niveau hôte) |
| `must_be_last_plugin` | 409 | greffon `last_plugin` suivi d'un autre (même décoché) |
| `mutually_exclusive` | 409 | deux membres cochés d'un groupe en politique `refuse` (le défaut) |
| `premium_required` | **402** | greffon Premium **coché**, ou upsampling demandé, sans Premium, **`bypass_all` compris** (la chaîne garde ses maillons et les anciennes clés les reçoivent) |

Ce qui dépend du flux ou de l'installation n'est **pas** refusé au `PUT`
(canaux, paquet désinstallé, droit perdu ensuite). La résolution l'éteint
étage par étage (§ 4).

## 4. Résolution à l'exécution (`tess::resolution`)

`ChaineTess::resoudre(&catalogue, &ContexteExecution)` rend, sans jamais
échouer :
- les étapes dans l'ordre réel ;
- le headroom réellement appliqué ;
- l'état du volume ;
- le badge.

### Ce que la résolution prend en entrée

Le `ContexteExecution` contient :
- le transport ;
- la source (`SourceFlux`, ou `None` si rien ne joue) ;
- le droit Premium ;
- la disponibilité de chaque greffon (paquet installé et activé) ;
- le mode Pure ;
- le volume de la zone : `software` (avec son gain en dB), `device` ou `fixed`.

### Ordre réel des étapes (Q9)

```
Source → Headroom → greffons (ordre de la liste) → Upsampling → Volume → Quantization → Output
```

- Volume logiciel **en flottant tout au bout**, au rendu : réactif en moins de
  50 ms (#7264), `volume.placement = "at_render"`.
- Dither et mise en forme du bruit : **étape finale séparée**.
- **Exception, upsampling vers DSD** :
  - ordre : `… → Upsampling → Volume → DsdModulator → Output` ;
  - `volume.placement = "before_dsd_modulator"` (réponse plus lente acceptée).
- **Maquettes A1 et A4** : elles dessinent le volume avant l'upsampling. L'écran
  doit montrer l'ordre ci-dessus (Q9), avec une ligne « Dither » finale en PCM.
  À corriger dans W1, W3 et W5.

### Étape résolue

Chaque étape porte :
- `code` ;
- `plugin_id` ;
- `fixed` ;
- `requested` : ce que la chaîne et la zone demandent ;
- `effective` : ce qui modifie réellement les échantillons ;
- `reason` et `detail` ;
- `params` : paramètres affichés, comme dB, filtre, rapport ou format de la
  source. Ceux propres à un greffon sont ajoutés par T16 ;
- `latency_frames` : rempli par l'exécuteur après la préparation (T03).

### Motifs d'extinction (`reason`)

**Blocages globaux** : ils éteignent toutes les étapes de traitement, dans cet
ordre de priorité.

| Motif | Cas |
|---|---|
| `outside_tess` | zone hors TESS ; le détail dit pourquoi |
| `chain_version_unsupported` | chaîne d'une version plus récente |
| `pure_mode` | mode Pure |
| `dsd_source_native` | source DSD jouée en natif ou en DoP |
| `bypass_all` | « Bypass all » |

**Motifs par étage** : ils n'éteignent que l'étage concerné, jamais toute la
chaîne. C'est le comportement de #7632.

| Motif | Cas |
|---|---|
| `unknown_plugin` | greffon absent du catalogue |
| `plugin_unavailable` | paquet désinstallé ou désactivé |
| `premium_required` | droit absent ou perdu |
| `stereo_only` | greffon stéréo seulement sur un flux d'un autre nombre de canaux ; le détail donne ce nombre |
| `replaced_in_group` | écarté dans un groupe exclusif |
| `unknown_value` | réglage fixe inconnu |
| `nothing_to_protect` | headroom automatique sans aucun traitement effectif |
| `volume_not_software` | volume réglé par l'appareil, ou volume fixe |
| `nothing_to_quantize` | rien ne modifie les échantillons ; le détail dit « Bit-perfect » |
| `not_in_service` | T16 : résolu effectif, mais l'étage ne tourne pas (exécuteur ou sonde de la sortie) |

### Badge (A4)

- **`bit_perfect`** : aucune étape ne modifie les échantillons. C'est le cas :
  - en mode Pure avec le volume à 0 dB (ou tenu à 100 % par le verrou
    `audiophile_lock_volume`) ;
  - avec une chaîne vide et le volume à 0 dB, ou un volume réglé par l'appareil ;
  - avec une source DSD native.
- **`processed_by_tess`** : « Processed by TESS · 64-bit ».
- **`outside_tess`** : zone hors TESS.
- **`altered_outside_tess`** (T16) : TESS ne touche à rien, mais le chemin
  n'est pas intact (transport, source), ou une zone hors TESS dont un
  traitement tourne malgré tout. Jamais « Bit-perfect » sur un chemin altéré.

### Comportements particuliers

- **Mode Pure** : greffons, headroom et upsampling sont éteints. Le volume
  logiciel, lui, **reste appliqué** (vérifié par T07 dans le code réel :
  `audio::audiophile::effective_volume`), avec la quantification qu'il
  exige ; seul le verrou `audiophile_lock_volume` (inactif par défaut) le
  tient à 100 %, et la zone redevient alors bit-perfect.
- **Volume et blocages** : le volume est un réglage de **zone**.
  - « Bypass all », Pure et une chaîne illisible ne l'éteignent pas.
  - Hors TESS ou en DSD natif, il n'est pas appliqué en logiciel.
- **Hors TESS**, la réponse n'annonce aucun headroom (`headroom_db = 0`) et
  aucun placement de volume (`placement = null`).

## 5. Préréglages et mode d'application (`tess::prereglages`, A2)

Un préréglage est une chaîne complète, nommée. Il garde chaque greffon, leur
ordre et tous leurs réglages.
- **Liste globale** : les préréglages sont communs à tout le serveur.
- **Préréglage actif par zone** : le préréglage chargé est propre à **chaque
  zone** (`active_preset` dans la réponse de la zone).
- Les préréglages propres à un greffon (`eq_presets`…) ne changent pas.

```json
{ "apply_mode": "immediately",
  "presets": [
    { "id": "pure", "name": "Pure — no processing", "builtin": true,
      "chain": { "version": 1, "bypass_all": true, "plugins": [], "fixed": { … } } },
    { "id": "evening-listening", "name": "Evening listening", "builtin": false, "chain": { … } } ] }
```

- **`apply_mode`** :
  - `immediately` : pause, puis reprise à la position (#8422) ;
  - `at_next_playback` : le changement attend la prochaine lecture ;
  - une valeur inconnue est lue comme `unknown` et traitée comme
    `at_next_playback`, pour ne jamais interrompre la lecture sans le vouloir.
- **Préréglage Pure** (« Pure — no processing ») : fourni par le serveur.
  Chaîne vide et `bypass_all`. Il peut être supprimé, pas modifié.
- **Charger un préréglage remplace la chaîne de la zone.** W4 propose
  d'enregistrer la chaîne courante avant.
- **`verifier` refuse** : un id invalide ou en double, un nom vide, un Pure
  modifié.
- **Droits** : ils sont vérifiés au **chargement**. Un préréglage Premium reste
  enregistré sur un compte redevenu gratuit.

## 6. Zones (`tess::zones`, Q8)

TESS s'applique partout où Tune **fabrique** le flux :
- `local`, `oaat` ;
- `dlna`, `openhome`, `bluos`, `squeezebox` (et `slimproto`) ;
- `chromecast`, `airplay` (et `airplay2`), `devialet_air` ;
- `browser`, `aes67`, `sendspin`.

Hors TESS, avec un motif affiché :

| Transport | `reason` |
|---|---|
| `hqplayer` | `hqplayer_own_processing` |
| `diretta` | `diretta_outside_tess` |
| `other` (non classé, ou ajouté plus tard) | `unclassified_transport` : jamais appliqué en silence ; T12 classe |

## 7. API (`tess::api`)

- **`GET /zones/{id}/tess`** → `zone_response`.
  - Un serveur sans TESS répond **404** : l'écran reste caché.
- **`PUT /zones/{id}/tess`** ← `chain` (lu strictement) → `zone_response`.
  - En cas de refus : `refusal`, en 400, 402 ou 409.

`zone_response` :

```json
{
  "contract_version": 1, "zone_id": 12, "transport": "oaat",
  "applicability": { "status": "applied" },
  "chain": { … }, "active_preset": "evening-listening",
  "headroom_db": -3.0,
  "volume": { "control": "software", "placement": "at_render", "db": -18.0 },
  "badge": "processed_by_tess",
  "signal_path": [ { "code": "source", "plugin_id": null, "fixed": true, "requested": true,
                     "effective": true, "reason": null, "detail": null,
                     "params": { "kind": "pcm", "sample_rate_hz": 96000, "bits": 24, "channels": 2 },
                     "latency_frames": null }, … ],
  "catalog": { "plugins": [ … ], "groups": [ { "id": "crossfeed_crosstalk", "policy": "refuse" } ] }
}
```

`refusal` : `{ "status": 400|402|409, "code": "…", "detail": "…", "plugin_id": "…"|null }`.
Le serveur y ajoute (T08, champs facultatifs) la forme des autres refus de
l'API : `error` (= `code`), `message` (= `detail`) et, sur un 402,
`upgrade_url`.

### Routes (T08, `tune-server/src/routes/zones/tess.rs`)

- **Authentification** : celle de toutes les routes de zone (couche
  `auth_middleware` de `/api/v1`).
- **`GET /tess/catalog`** → `catalog_response` :
  `{ contract_version, premium, catalog, plugins: [ { id, entitled, available, reason } ] }`.
  - `catalog` : la référence, les **schémas de réglages des caisses du SDK**
    (`sdk/tune-plugin-*/schemas/config.json`, sans `enabled`, porté par le
    maillon), les paquets natifs tiers chargés (Premium), et le groupe
    crossfeed en politique **`refuse`** (décision du 18/11).
  - `reason` : `plugin_unavailable` (pas installé, désactivé ou pas branché,
    comme `declick-live` aujourd'hui) avant `premium_required`.
  - 200 = la capacité TESS ; 404 = serveur sans TESS.
- **`GET /zones/{id}/tess`** : la chaîne lue par `lire_zone` (P2 appliqué sans
  Premium), résolue avec le transport, la source qui joue, le mode Pure, le
  volume (logiciel, appareil ou fixe) et la disponibilité de chaque greffon.
  Zone inconnue : 404 `{"error": "zone_not_found"}`.
- **`PUT /zones/{id}/tess`**, dans cet ordre :
  1. corps lu strictement (`depuis_corps`) : 400 `invalid_body` ;
  2. **fusion** : un champ absent du corps garde sa valeur enregistrée
     (`fixed` champ par champ ; `plugins`, s'il est présent, remplace la
     liste). Un client qui ignore un champ plus récent ne l'efface donc pas ;
  3. P2 inverse (et D1) : sans Premium, le graphique converti renvoyé tel
     quel redevient le paramétrique, ou la correction de pièce mesurée,
     enregistré ; un graphique modifié le remplace ;
  4. **origine D1** : l'`origin` d'un `room-correction` est réécrite par le
     serveur d'après `room_profile_{id}` (`origines_par_le_serveur`), jamais
     crue du corps. Les marques de lecture (`replaced_in_group`,
     `inert_on_network`) sont tenues par `ecrire_chaine` ; elles ne marquent
     que des maillons décochés et ne lèvent aucun droit ;
  5. validation : 409 (forme, exclusivité du groupe crossfeed, version),
     puis 402 sur ce qui **change** : un maillon ou un upsampling identique à
     la chaîne enregistrée n'est pas revérifié (la résolution l'éteint,
     `premium_required`) ; perdre le Premium ne bloque pas l'édition du reste ;
     une correction de pièce d'origine `estimation` (C5, sans micro) est Free.
     Le droit étant contrôlé sous `bypass_all` comme ailleurs, l'exemption
     vaut aussi pour une chaîne enregistrée sous `bypass_all`.
     Un maillon absent du catalogue (paquet tiers installé mais non chargé)
     renvoyé **inchangé** est conservé tel quel au lieu d'un 409
     `unknown_plugin`, sous la même règle de droit (Premium, comme tout
     greffon tiers) ; modifié, il reste refusé en 409 ;
  6. `ecrire_chaine` (sous `verrou_de_zone`), puis rafraîchissement de la
     sortie vivante pour chaque ancienne clé réécrite. Un `PUT` à la fois par
     zone, de la lecture à l'écriture.
  - Une chaîne enregistrée d'une version plus récente n'est pas réécrite :
    409 `unsupported_version`.
- `active_preset` : lu sous `zone_{id}_tess_active_preset` (posé par T17).

## 8. Engagements des lots suivants

Ces points ne sont pas codés dans T01. Ils sont fixés ici pour que chaque lot
s'y tienne.

### T07 : étages fixes (`tess::etages_fixes`)

- **Headroom** (`EtageHeadroom`) : headroom de TESS, ReplayGain et gain de
  zone en un gain d'entrée, jamais au-dessus de 0 dB ; la part positive du
  ReplayGain et du gain de zone part au rendu, sous le rabot du volume.
- **Bilan de niveau** (`BilanDeNiveau`) : l'automatique (−3 dB) ne s'empile
  pas sur la réserve déclarée par les greffons (−12 dB d'AutoEq ⇒ 0 dB de
  plus) ; la compensation rend headroom et réserves UNE fois, par le volume,
  rabotée à l'unité ; jamais le ReplayGain ni le gain de zone. La réserve de
  3 dB du suréchantillonneur disparaît sous TESS (T11a).
- **Volume** (`EtageVolume`) : `f64`, au rendu, consigne relue à chaque bloc
  et rampe de 20 ms.
- **Étape finale** : `noise_shaped` = `QuantificationMiseEnForme` ; TPDF et
  `off` = `QuantificationPcm` (T03) ; DSD = modulateur de T11b, volume juste
  avant.

### T03, T04 et T07 : l'anneau et la quantification

Le volume au rendu suivi de la quantification exige que **l'anneau entre le
producteur et le rendu transporte des `f64`**, et que la quantification
(`EtapeFinale`) se fasse dans le fil de rendu.
- Aujourd'hui, l'anneau natif porte des mots entiers, et
  `gain_au_rendu_7264` requantifie avec un dither : cela fait **deux**
  quantifications.
- Un anneau en `f32` violerait Q5 : il est exclu.
- La mémoire de l'anneau double. À mesurer sur le Pi 4 (T18).

### T02 : migration (fait, `tess::migration`)

- **Stockage** : la chaîne sous `zone_{id}_tess_chain` ; l'**ombre** sous
  `zone_{id}_tess_chain_sync` (valeur brute de chaque ancienne clé à la
  dernière écriture de la chaîne, construite à partir de ce qui a été lu puis
  écrit, jamais relue). Une ancienne clé dont le sens JSON diffère de l'ombre
  a été écrite par une ancienne route : elle l'emporte pour ce qu'elle
  exprime. Le reste (ordre, greffon décoché, Headphone EQ, DSD, headroom,
  dither, `bypass_all`) reste celui de la chaîne.
- **La chaîne enregistrée ne dépend pas du droit** : `migrer_zone` n'applique
  jamais P2 ; P2 n'existe qu'à la lecture (`lire_zone(…, premium = false)`).
- **Verrou par zone** (`verrou_de_zone`) : T03 (migration au démarrage) et
  T08 (`PUT /zones/{id}/tess`) n'écrivent la chaîne que par `migrer_zone` et
  `ecrire_chaine`, qui le prennent. Les anciennes routes ne le prennent pas :
  l'ombre suffit pour elles.
- **Correspondance** :

| Ancienne clé | Chaîne |
|---|---|
| `zone_{id}_eq_profile` | une façade d'égaliseur, `enabled` du profil, `settings` = profil sans `enabled` (sa réserve `headroom_mode` comprise) : profil de pièce (`room_profile_{id}`) → `room-correction`, avec son origine `measured` ou `estimation` (D1) ; sans bande ou courbe de grille ISO 10/15/31 → `equalizer-graphic` ; sinon `equalizer-headphone` si la chaîne l'était déjà, sinon `equalizer-parametric` |
| `ir_path_{id}` | `convolution`, coché, `settings.ir_path` |
| `zone_{id}_crossfeed` | `crossfeed` (`enabled`, reste en `settings`) |
| `zone_{id}_native_plugin_{p}` | greffon `{p}` (Crossfeed Pro, Crosstalk, paquets tiers) |
| `zone_{id}_mono_downmix` | `mono-downmix`, coché si `"true"` sur une sortie locale ; décoché avec `inert_on_network` sur une zone réseau (D2) |
| `zone_{id}_channel_remap` | `channel-remap`, en dernier |
| `zone_{id}_upsampler` | `fixed.upsampling` : `pcm` (`filter`, `max_ratio`, `max_rate_hz`) si `enabled`, sinon `off` |
| `zone_{id}_audiophile`, `zone_{id}_strict_bitperfect` | hors chaîne : `ContexteExecution.pure` |
| `zone_{id}_level_compensation` (#4685) | hors chaîne : `LectureZone::compensation_de_niveau` (vrai sauf `"false"`) |
| colonne `zones.dsd_mode` | hors chaîne : `LectureZone::dsd_mode`, `lecture_dsd` |
| `zone_{id}_gain_trim_db` | hors chaîne : gain de zone, porté par l'étage headroom (T07), comme le ReplayGain ; peut être positif, donc pas un headroom manuel |

- **Engagements pour T07** :
  - `level_compensation` compense le gain MOYEN de l'égaliseur et du
    crossfeed, au volume ; ce n'est pas le headroom (réserve de crête). T07 la
    garde : l'interrupteur coupé reste coupé.
  - La réserve propre de l'égaliseur (`headroom_mode` `safe` ou `realistic`,
    avec son limiteur) reste dans ses réglages. En `Auto`, l'étage headroom
    ne l'additionne pas à ses −3 dB : la réserve appliquée est la plus forte
    des deux, jamais la somme.
- **Engagement pour T16** : `dsd_mode` dit ce que la zone fait d'une SOURCE
  DSD (`native` et `dop` : hors traitement, `dsd_source_native` ; `pcm` :
  converti avant la chaîne ; `auto` : selon le périphérique).
  `fixed.upsampling = dsd` ne concerne que les sources PCM.
- **Ordre d'une chaîne migrée** : celui de la lecture d'aujourd'hui :
  égaliseur, convolution, crossfeed, `crossfeed-*`, crosstalk, autres paquets,
  repli mono, réaffectation.
- **Groupe crossfeed** (décision B) : aucune chaîne migrée n'est refusée et
  rien ne change à l'oreille. Si les anciennes clés cochent plusieurs membres,
  le gagnant est celui qui traite aujourd'hui : le plus prioritaire (Crosstalk
  > `crossfeed-*` > crossfeed intégré, à égalité le premier) parmi les
  membres en service (paquet installé et activé, greffon intégré activé ; le
  droit Premium n'entre pas en compte, la chaîne enregistrée n'en dépend
  pas). Les autres sont DÉCOCHÉS, réglages gardés, avec
  `"replaced_in_group": "<gagnant>"` ; leur ancienne clé reste cochée (l'ancien
  moteur applique déjà la priorité). Si le gagnant n'est plus coché (ancienne
  route), le membre écarté est recoché, puis la règle s'applique de nouveau.
- **P2** : à la lecture sans Premium, un `equalizer-parametric` **coché**
  devient un `equalizer-graphic` au plus près, sur la grille réglée
  (`eq_expert_bands`), ±12 dB au plus ; deux courbes si les canaux diffèrent.
  Décoché, il reste tel quel.
- **Écriture croisée** : `ecrire_chaine` met à jour les anciennes clés que la
  chaîne exprime, et seulement celles dont le sens change. Un greffon retiré
  ou décoché est décoché dans son ancienne clé (réglages gardés) ; la
  convolution, sans état « décoché », efface `ir_path_{id}` (le chemin reste
  dans la chaîne) ; un upsampling DSD éteint l'ancien suréchantillonneur. Le
  premier profil d'égaliseur réécrit est gardé sous
  `zone_{id}_eq_profile_before_tess`. Le bilan rend les clés modifiées, à
  rafraîchir par la route (T08).
- **Lecture** : tolérante comme aujourd'hui pour les booléens non canoniques
  (`"on"`, `"yes"`, `"enabled": "yes"` valent faux, sans refus) ; une ombre
  illisible vaut une ombre absente. Refus explicite (`CleCorrompue`, qui
  nomme la clé) seulement pour une clé que son lecteur actuel ne sait pas
  lire (JSON invalide, `eq_profile`, `channel_remap` ou `upsampler`
  illisibles) et pour une chaîne enregistrée illisible. T03 garde alors le
  chemin de lecture actuel pour la zone et journalise le refus.
- **Migration** : `migrer_zone` n'écrit que la chaîne et l'ombre, jamais une
  ancienne clé. Idempotente. Une chaîne d'une version plus récente n'est
  jamais réécrite. Rien n'est encore branché au démarrage ni dans la lecture
  (T03).
- **Export et import de configuration** (décision B de Bertrand, 19/11) :
  l'export (`config_export`, format 2) porte, dans chaque zone,
  `zone_{id}_tess_chain` : la chaîne que le serveur LIT pour la zone
  (`lire_zone`, neutre vis-à-vis du droit), donc toujours d'accord avec les
  anciennes clés qui partent avec elle, et calculée pour une zone jamais
  migrée. L'ombre `zone_{id}_tess_chain_sync` ne part plus. L'import
  (`POST /system/config/import` et restauration cloud, par
  `config_export::planifier`/`appliquer`) **réécrit** cette chaîne par
  `ecrire_chaine` sous le numéro de la zone d'arrivée, après les autres
  réglages de la zone : lecture stricte (`ChaineTess::depuis_corps`),
  origine des corrections et marques recalculées par le serveur, anciennes
  clés et ombre réécrites. L'ombre d'un fichier n'est jamais importée, sans
  avertissement. Une chaîne illisible ou d'une version plus récente n'est
  pas importée (avertissement) : la chaîne se reconstruit alors depuis les
  anciennes clés. Le profil de pièce de la zone (`room_profile_{id}`) est
  restauré comme **mesuré**, `zone_id` remplacé par le numéro d'arrivée,
  index tenu par `room_correction::save_profile`, avec un avertissement
  (« restored as measured »), comme `config_backup`. Il est écrit AVANT la
  chaîne, qui en tire son origine. `room_profile_index` dans les réglages
  généraux d'un fichier au format 2 est ignoré sans avertissement (il se
  reconstruit). Un aller-retour normal ne donne donc plus d'avertissement
  « server-managed ». L'aperçu compare la chaîne du fichier à celle que le
  serveur lit (origine exclue) : réimporter le même fichier ne change rien.
- **Décisions de Bertrand (18/11)** :
  - **D1** (correction de pièce) : un égaliseur de pièce posé dans
    `eq_profile` devient `room-correction`, avec son origine dans ses
    réglages (`"origin"`, jamais écrit dans `eq_profile`).
    - **Mesurée au micro** (`"measured"`) : Premium pour tous, plus de droit
      acquis. Pour un compte gratuit, elle est **convertie** en égaliseur
      graphique au plus près à la LECTURE, comme P2 ; jamais dans la chaîne
      enregistrée ni dans `eq_profile`.
    - **Calculée sans micro** (C5, #7919, `"estimation"`) : Free, active sans
      Premium, jamais convertie (`correction_estimee`, honoré par la
      résolution).
    - **Origine** : tout profil créé désormais l'enregistre
      (`RoomProfile.origin`, `"measured"` ou `"estimation"`) : le greffon de
      calibration selon sa route (mesure C2 ou estimation C5), la route
      manuelle `POST /room-correction/profiles/{zone}` toujours en
      `"measured"`. Les profils anciens ne l'ont pas. On lit
      `room_profile_{id}` : `"origin"` l'emporte (aucun autre champ n'est
      cru, `"estimation": bool` compris) ; sinon le nom par défaut d'une
      estimation C5 (« Estimation {id} ») ; sinon **mesurée**. Les bornes de C5 ne servent
      pas d'indice (le mode expert #8332 les relève). Dans le doute, la
      correction est donc traitée en Premium : convertie, jamais détruite.
      Sortir C5 de la garde Premium du greffon reste la tâche Codex 28.
    - **Clés tenues par le serveur** : `room_profile_*` (index compris),
      `zone_{id}_tess_chain` et `zone_{id}_tess_chain_sync` sont refusées
      par `PATCH /system/config` (`403 server_managed_settings`, admin
      compris) ; elles ne s'écrivent que par leurs routes. L'import de
      configuration (`POST /system/config/import`, et la restauration cloud
      qui passe par le même plan) ne les écrit jamais telles quelles : dans
      la partie zones d'un fichier au format 2, la chaîne et le profil de
      pièce sont réécrits par le serveur (voir « Export et import de
      configuration » ci-dessus) ; partout ailleurs (réglages généraux,
      ancien format à plat), il les ignore et le dit dans ses
      avertissements. La restauration d'une
      sauvegarde (`config_backup`) ignore ces clés dans ses réglages
      généraux, n'écrit que des clés de profil dans sa section des profils,
      et restaure chaque profil comme **mesuré** (l'origine d'un fichier ne
      se vérifie pas ; converti, jamais détruit, pour un compte gratuit), en
      le disant.
    - **L'origine appartient au serveur** : `lire_zone` la recalcule à
      chaque lecture (jamais tirée du stockage brut de la chaîne, y compris
      pour une chaîne d'une version plus récente, rendue sans les anciennes
      clés mais avec l'origine recalculée et la conversion d'un compte
      gratuit), et
      `ecrire_chaine` la réécrit
      d'après `room_profile_{id}` quoi que dise le corps (`"estimation"`
      seulement si les bandes sont celles d'une estimation de la zone,
      `"measured"` sinon). Un `PUT` ne peut donc jamais lever le droit
      Premium en se disant « sans micro ».
  - **D2** (`mono-downmix`) : 11ᵉ greffon, Free. Sur une zone réseau, où
    l'interrupteur actuel était inerte (#3254, sortie locale seulement), il
    est **décoché d'office, une seule fois, à la migration**, avec
    `"inert_on_network": true` : aucun changement audible, et l'ancienne clé
    n'est pas touchée. Coché ensuite (TESS ou ancienne route), il agit
    partout. Si la zone passe ensuite sur une sortie locale, où
    l'interrupteur (resté à `"true"`) agit, il est recoché à la lecture.
    Une zone locale qui passe sur le réseau suit la règle de la migration :
    son mono est décoché d'office avec `inert_on_network` (l'ancien moteur
    l'ignorait sur le réseau) ; recoché ensuite à la main, il agit. La
    nature de la sortie à la dernière synchronisation est gardée dans
    l'ombre (entrée `"output"` : `"local"` ou `"network"`) ; la règle ne
    joue que si l'interrupteur mono n'a pas bougé depuis.
- **Les marques de la lecture appartiennent au serveur** (T02 suites, pour
  T08) : `origin` (D1) est réécrite d'après `room_profile_{id}` ;
  `replaced_in_group` (décision B) et `inert_on_network` (D2) ne sont gardées
  par `ecrire_chaine` que si le maillon, décoché, porte DÉJÀ la même marque
  dans la chaîne que le serveur lit pour la zone (migration à la lecture
  comprise), et, pour `replaced_in_group`, si le gagnant nommé reste coché.
  Sinon la marque est retirée et le maillon décoché est projeté normalement
  (son ancienne clé est décochée). Un client qui renvoie les réglages lus ne
  peut donc ni garder cochée l'ancienne clé d'un membre écarté après avoir
  décoché le gagnant, ni faire recocher en local un mono décoché à la main.
- **Décidé le 18/11 au soir** (Bertrand : « fais au mieux » ; principe :
  aucun changement audible dû à la migration, aucun droit Premium sans le
  payer ; `decisions-tess.md`) :
  - **I3** : pas de copie de base réelle de testeur (données personnelles).
    Le test `i3_relecture_d_une_copie_de_base_reelle` reste sauté et
    recensé dans le garde-fou 2816 ; la grande base synthétique couvre la
    migration. Ce n'est pas un préalable à la fusion.
  - **D2, zone locale passée sur le réseau** : mono décoché d'office
    (`inert_on_network`), recoché à la main il agit (voir D2 ci-dessus).
  - **D2, zone réseau passée en local** : recoché à la lecture. Confirmé.
  - **D1, origine** : la règle du nom « Estimation {id} » est acceptée
    pour les profils anciens ; tout profil créé enregistre son origine, et
    la règle du nom ne s'applique plus qu'aux profils sans origine.

### T05 : manifeste

- `exclusive_group` dans le manifeste des paquets.
- Le préfixe `crossfeed-` reste reconnu pour les paquets déjà publiés.

### T08 : API (fait, § 7)

- Les routes ; le corps du `PUT` passe par `depuis_corps` puis `valider`.
- Les schémas de réglages des caisses du SDK dans le catalogue.
- `active_preset` par zone.

### T16 : chemin du signal

- Les paramètres propres à chaque greffon dans `params` (fichier de RI, bandes,
  préréglage), et `latency_frames`.
- Le chemin du signal ne ment jamais : chaque étape `effective` correspond à un
  étage réellement en service.
- Fait (T16) : `tess::chemin::chemin_du_signal` rapproche la résolution de ce
  qui tourne (`EnService` : liste exacte de l'exécuteur, ou sonde de la sortie
  tant que l'exécuteur n'est pas branché par zone) ; le réel l'emporte dans
  les deux sens et chaque écart est publié dans `discrepancies`
  (`requested_not_running`, `running_not_requested`). Le serveur l'ajoute
  sous `signal_path.tess` (zone, liste des zones, instantané WS). Ordre
  publié : `source → headroom → greffons (ordre réel) → upsampling → volume →
  quantization (« Dither ») → output`, ou `… → volume → dsd_modulator →
  output` vers DSD. Les étapes contournées gardent leur motif (`bypass_all`,
  `pure_mode`, `dsd_source_native`, `volume_not_software`…) ; un greffon
  décoché par la marque `inert_on_network` (D2) le dit dans `detail`.

### T19 : droits

- Lien droit ↔ `Feature` de `license.rs` par le champ `feature` : Parametric EQ
  et Declick à fixer, et passage du crossfeed en Free.
- `ContexteValidation.premium` et `ContexteExecution.premium` deviendront un
  test par `Feature`.

### W1 à W5 : écran

- L'écran montre l'ordre réel de Q9.
- Les niveaux suivent le catalogue, pas la maquette A3.
- Charger un préréglage remplace la chaîne courante.

## 9. Preuves (tests)

Commande : `cargo test -p tune-core --lib tess::`.

- **Trait d'étage**, testé sur des étages réels de test :
  - propagation du format, de la taille de bloc, des capacités préallouées et
    de la latence (ramenée à la fréquence de sortie) ;
  - refus d'un étage « sur place » qui change de format ;
  - tampon trop petit refusé sans panique ;
  - vidange de la queue ;
  - retard conservé en contournement ;
  - `ErreurEtage` est `Copy`.
- **Compatibilité** :
  - réponse, descripteur, applicabilité, étape et préréglages plus récents
    (champs et valeurs inconnus) lus par les types Rust et par le schéma de
    lecture ;
  - contre-épreuve : le schéma strict du serveur les refuse.
- **Corps strict** : JSON illisible, champ inconnu et valeur inconnue donnent
  un 400. Le format est figé par un document littéral.
- **Upsampling** : chaque filtre et chaque rapport du SDK passent. 3, 32, 1, un
  filtre inconnu et un plafond hors bornes sont refusés.
- **Résolution** :
  - ordre Q9 en PCM (upsampling effectif avant le volume) et exception DSD ;
  - zone hors TESS : ni headroom ni ordre DSD annoncés ;
  - dégradation par étage (droit perdu, paquet absent, 5.1 sur stéréo) ;
  - priorité du groupe crossfeed (#7616) ;
  - Pure et chaîne vide : bit-perfect ; contre-épreuve : un volume à −1 dB
    requantifie ;
  - volume de l'appareil ; source DSD native ; chaîne d'une version plus
    récente ; headroom automatique.
- **Code réel** :
  - les `feature` du catalogue existent dans `license.rs` ;
  - les membres du groupe correspondent à `est_de_la_famille_du_crossfeed` et
    à `ID_CROSSTALK` ;
  - les filtres et rapports viennent du SDK du suréchantillonneur.
- **Codes de refus** : `ErreurChaine::rang` est un `match` exhaustif.
  `exemples()` couvre chaque rang, et le schéma tire ses codes de `exemples()`.
