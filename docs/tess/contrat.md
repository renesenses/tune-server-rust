# TESS : le contrat (lot T01)

*Tune Engine for Sound Shaping.* Ce document décrit les types du module
`tune-core/src/tess/`. Ce sont des **types seulement** : rien n'est branché dans
la lecture. Les lots suivants (T02 modèle et migration, T03/T04 exécuteur,
T05 SDK F64, T07 étages fixes, T08 API, T12 zones réseau, T16 chemin du signal,
W1 écran) s'appuient dessus.

Version du contrat : **1** (`tess::VERSION_CONTRAT`). Version du format de
chaîne enregistrée : **1** (`tess::VERSION_CHAINE`).

## 1. Étage de traitement (`tess::etage`)

```rust
pub trait EtageTess: Send {
    fn id(&self) -> &str;
    fn preparer(&mut self, entree: FormatFlux) -> Result<FormatFlux, ErreurEtage>;
    fn traiter(&mut self, tampon: &mut TamponTess) -> Result<(), ErreurEtage>;
    fn latence_trames(&self) -> u32 { 0 }
    fn bypass(&self) -> bool;
    fn definir_bypass(&mut self, bypass: bool);
    fn reinitialiser(&mut self) {}
}
```

- `TamponTess` porte des échantillons **entrelacés en `f64`**, pleine échelle
  ±1,0, avec le nombre de canaux et la fréquence. Aucune conversion `f32` ni
  entière entre deux étages (décision Q5) ; la seule quantification est l'étape
  finale de la chaîne.
- Le tampon est un `Vec` : la réaffectation des canaux change sa longueur.
- En `bypass`, un étage ne touche pas le tampon : c'est ce qui garde une chaîne
  vide ou contournée bit-perfect.
- `latence_trames` est annoncée à l'hôte (anticipation du Declick temps réel,
  convolution partitionnée) pour l'alignement multi-points et le chemin du signal.

Preuve par le type (critère « 64 bits réels ») : le test
`le_tampon_d_un_etage_est_en_f64` compare le `TypeId` des échantillons à `f64`.

## 2. Descripteur de greffon (`tess::greffon`)

Un greffon TESS est un traitement du SDK natif : référence compilée dans le
serveur (`origin: "reference"`) ou paquet `.tuneplugin` signé
(`origin: "package"`). Pas de troisième système de greffons.

| Champ | Sens |
|---|---|
| `id` | stable ; minuscules ASCII, chiffres, tirets ; 64 caractères au plus |
| `name` | nom d'affichage anglais, traduit par les clients |
| `category` | groupe de la fenêtre « Add plugin » (maquette A3) : `equalisation`, `spatial`, `room_and_filters`, `restoration_and_routing` |
| `entitlement` | `free` ou `premium` |
| `origin` | `reference` ou `package` |
| `settings_schema` | schéma JSON (2020-12) des réglages ; vient de la caisse du SDK (fonction `schemas`) ou du manifeste du paquet |
| `placement.channels` | `any` (n'importe quel nombre, inchangé), `stereo_only` (seulement s'il le déclare), `remaps` (change le nombre de canaux) |
| `placement.position` | `free` (ordre libre) ou `last_plugin` |
| `placement.exclusive_group` | deux greffons **actifs** d'un même groupe ne coexistent pas |

Règle interne : `remaps` impose `last_plugin` (aucun greffon ne voit un flux
réaffecté). L'hôte vérifie dans les réglages : objet ou `null`, clés `required`
présentes, aucune clé inconnue si `additionalProperties: false`. Le reste est
vérifié par le greffon.

### Catalogue de référence (v1.0)

| id | Nom | Catégorie | Droit | Origine | Canaux | Place | Groupe exclusif |
|---|---|---|---|---|---|---|---|
| `equalizer-graphic` | Graphic EQ | equalisation | Free | référence | any | libre | — |
| `equalizer-parametric` | Parametric EQ | equalisation | **Premium** (Q3) | référence | any | libre | — |
| `equalizer-headphone` | Headphone EQ | equalisation | Free | référence | any | libre | — |
| `crossfeed` | Crossfeed | spatial | **Free** (Q2 révisée) | référence | stereo_only | libre | `crossfeed_crosstalk` |
| `crossfeed-pro` | Crossfeed Pro | spatial | Premium | paquet | stereo_only | libre | `crossfeed_crosstalk` |
| `crosstalk` | Crosstalk | spatial | Premium | paquet | stereo_only | libre | `crossfeed_crosstalk` |
| `convolution` | Convolution | room_and_filters | Premium | référence | any | libre | — |
| `room-correction` | Room correction | room_and_filters | Premium | référence | any | libre | — |
| `declick-live` | Declick | restoration_and_routing | **Premium** (P1) | référence | any | libre | — |
| `channel-remap` | Channel remapping | restoration_and_routing | Free | référence | remaps | dernier | — |

Les niveaux de la maquette A3 sont **périmés** sur deux lignes (Parametric EQ
« TIER TBD », Declick « FREE ») : ce tableau, qui suit les décisions Q2 (révisée
le 17/11 : Crossfeed simple Free, Crossfeed Pro Premium), Q3 et P1, fait foi.
Le passage du crossfeed en Free dans `license.rs` et `premium_plugins.rs`
n'est pas fait ici (lot T19).

- Les trois égaliseurs sont trois façades d'**un seul** moteur (lot T06).
- `declick-live` est distinct de l'outil de fichiers `declick` (Studio, Q4).
- Le suréchantillonneur n'est **pas** un greffon : c'est un étage fixe.
- Crossfeed Pro et Crosstalk viennent de dépôts privés : le contrat public n'en
  connaît que l'id, le droit et la place.
- Les schémas de réglages du catalogue de référence sont minimaux
  (`{"type":"object"}`) : T08 y branche ceux des caisses du SDK.
- P2 (compte gratuit avec un profil paramétrique) : la conversion vers le
  graphique est le travail de T02/T19. Le contrat garde un greffon Premium
  **décoché** dans la chaîne d'un compte gratuit sans refus.

## 3. Chaîne d'une zone (`tess::chaine`)

Enregistrée sous la clé `zone_{id}_tess_chain` (lot T02), échangée telle quelle
par `PUT /zones/{id}/tess`.

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
    "upsampling": { "mode": "off" },
    "quantization": { "dither": "tpdf" }
  }
}
```

- Seul `version` est obligatoire ; `bypass_all`, `plugins` et `fixed` ont des
  valeurs par défaut (chaîne vide, headroom auto, upsampling éteint, TPDF).
- Tout champ inconnu est **refusé** à la lecture.
- `headroom` : `{"mode":"auto"}` (−3 dB dès qu'un greffon ou l'upsampler est
  actif, 0 dB sinon) ou `{"mode":"manual","db":x}` avec x dans [−24 ; 0].
  Il porte aussi le ReplayGain et le gain de zone, réglés comme aujourd'hui.
- `upsampling` (Premium) : `off`, `{"mode":"pcm","factor":n}` avec n dans
  [2 ; 16], ou `{"mode":"dsd","rate":"dsd64"|"dsd128"|"dsd256"}`.
- `quantization.dither` : `tpdf` ou `noise_shaped`.
- Le **volume** n'est pas dans la chaîne : il reste un réglage de zone ; le
  contrat fixe seulement sa place.
- `bypass_all` : greffons, headroom et upsampling contournés ; restent le volume
  et la quantification (volume à 0 dB ⇒ bit-perfect).
- Un greffon décoché garde sa place et ses réglages et ne traite rien.

### Ordre réel des étapes (décision Q9)

```
Source → Headroom → greffons (ordre de la liste) → Upsampling → Volume → Quantization → Output
```

- Volume **en flottant tout au bout**, appliqué au rendu : réactif en moins de
  50 ms (#7264), `volume_placement: "at_render"`.
- Dither et mise en forme du bruit : **étape finale séparée** (`quantization`).
- **Exception, upsampling vers DSD** : le volume passe juste avant le modulateur
  sigma-delta, qui tient lieu d'étape finale :
  `… → Upsampling → Volume → DsdModulator → Output`,
  `volume_placement: "before_dsd_modulator"` (réponse plus lente acceptée).
- L'écran TESS montre **cet** ordre. La maquette A1 dessine « Volume control »
  avant « Upsampling · LAST » : c'est l'ordre ci-dessus qui fait foi (Q9 :
  « l'écran TESS montre l'ordre réel »). De même, le chemin du signal de la
  maquette A4 doit montrer Upsampling avant Volume, puis une ligne « Dither »
  finale en PCM (ou le modulateur en DSD). À corriger dans W1, W3 et W5.
- Le badge de A4 (« Processed by TESS · 64-bit » ou « Bit-perfect ») et les
  paramètres affichés par étape relèvent de T16 : ce contrat fournit l'ordre et
  l'état `requested` / `effective` de chaque étape.

`ChaineTess::ordre_des_etapes()` rend cette liste ; chaque étape porte `code`,
`plugin_id`, `fixed` et `active`.

### Validation (`ChaineTess::valider`)

Contexte : nombre de canaux à l'entrée s'il est connu (`None` à
l'enregistrement : « stéréo seulement » est alors vérifié à l'exécution) et
droit Premium du compte.

| Code | HTTP | Règle |
|---|---|---|
| `unsupported_version` | 409 | `version` ≠ 1 |
| `headroom_out_of_range` | 409 | headroom manuel hors [−24 ; 0] dB ou non fini |
| `upsampling_factor_out_of_range` | 409 | facteur PCM hors [2 ; 16] |
| `unknown_plugin` | 409 | id absent du catalogue |
| `duplicate_plugin` | 409 | même id deux fois |
| `invalid_settings` | 409 | réglages non conformes au schéma (niveau hôte) |
| `must_be_last_plugin` | 409 | greffon `last_plugin` suivi d'un autre (même décoché) |
| `premium_required` | **402** | greffon Premium **actif** ou upsampling actif sans Premium |
| `mutually_exclusive` | 409 | deux greffons actifs d'un même groupe exclusif |
| `stereo_only` | 409 | greffon `stereo_only` actif sur un flux qui n'a pas 2 canaux |

## 4. Préréglages et mode d'application (`tess::prereglages`, maquette A2)

« A preset keeps every plugin, their order and all their settings » : un
préréglage est une chaîne complète, nommée. Les préréglages sont **globaux** ;
chaque zone garde sa chaîne. Les préréglages propres à un greffon (`eq_presets`,
`crossfeed_presets`…) ne changent pas.

```json
{
  "active": "evening-listening",
  "apply_mode": "immediately",
  "presets": [
    { "id": "pure", "name": "Pure — no processing", "builtin": true,
      "chain": { "version": 1, "bypass_all": true, "plugins": [], "fixed": { … } } },
    { "id": "evening-listening", "name": "Evening listening", "builtin": false,
      "chain": { … } }
  ]
}
```

- `apply_mode` : `immediately` (pause puis reprise avec les nouveaux réglages,
  relance à la position #8422) ou `at_next_playback`.
- « Pure — no processing » est fourni par le serveur (`builtin`, id `pure`) :
  chaîne vide et `bypass_all`. Il peut être supprimé, pas modifié.
- `PreReglages::verifier` refuse : id invalide ou en double, nom vide, préréglage
  actif inexistant, Pure modifié. La chaîne d'un préréglage est validée contre le
  catalogue et les droits **au chargement** : un préréglage Premium reste
  enregistré sur un compte redevenu gratuit.

## 5. Zones (`tess::zones`, décision Q8)

TESS s'applique partout où Tune **fabrique** le flux : `local`, `oaat`, `dlna`,
`openhome`, `bluos`, `squeezebox` (et `slimproto`), `chromecast`, `airplay`
(et `airplay2`), `devialet_air`.

Hors TESS, avec un motif affiché sur l'écran :

| Transport | `reason` |
|---|---|
| `hqplayer` | `hqplayer_own_processing` |
| `diretta` | `diretta_outside_tess` |
| tout autre (`other`) | `unclassified_transport` (jamais appliqué en silence ; T12 classe) |

« Dans TESS » ne veut pas dire « livré » : l'état effectif étape par étape est
publié dans `signal_path` (§ 6) et affiné par T08 et T12.

## 6. API (`tess::api`)

- `GET /zones/{id}/tess` → `zone_response` ; un serveur sans TESS répond **404**,
  ce qui garde l'écran TESS caché côté web.
- `PUT /zones/{id}/tess` ← `chain` → `zone_response`, ou `refusal` (402 / 409).

`zone_response` :

```json
{
  "contract_version": 1,
  "zone_id": 12,
  "transport": "oaat",
  "applicability": { "status": "applied" },
  "chain": { "version": 1, "bypass_all": false, "plugins": [], "fixed": { … } },
  "headroom_db": 0.0,
  "volume_placement": "at_render",
  "signal_path": [
    { "code": "source", "plugin_id": null, "fixed": true, "requested": true,
      "effective": true, "reason": null, "detail": null }
  ],
  "catalog": { "plugins": [ … ] }
}
```

- `signal_path[]` : `requested` (ce que la chaîne demande), `effective` (ce qui
  est appliqué), `reason` / `detail` quand ils diffèrent (modèle de
  `crossfeed_status`). Sur une zone hors TESS, headroom, greffons et upsampling
  demandés sont `effective: false` avec le motif de la zone.
- `refusal` : `{ "status": 402|409, "code": "…", "detail": "…", "plugin_id": "…"|null }`.

Le schéma JSON complet est rendu par `tess::schema_json()` (définitions
`chain`, `zone_response`, `refusal`, `catalog`, `presets`, …). Les tests vérifient que ce
que le serveur sérialise y est conforme, et qu'il refuse des documents invalides.

## 7. Preuves (tests)

`cargo test -p tune-core tess::` :

- aller-retour JSON : chaîne (toutes les variantes d'étages fixes), catalogue,
  transports et applicabilité, réponse de zone, refus ;
- format figé : un document JSON littéral relu puis réécrit à l'identique ;
- contraintes de place, chacune avec sa contre-épreuve : crossfeed / Crossfeed
  Pro / Crosstalk exclusifs ; réaffectation en dernier ; stéréo seulement si
  déclaré ; droits Premium (402) ; chaînes invalides refusées (409) ; champs
  inconnus refusés ;
- ordre Q9, y compris l'exception DSD ; headroom automatique ; Bypass all ;
- préréglages : aller-retour, Pure sans traitement, listes incohérentes refusées ;
- tampon en `f64` (par le type) et double précision conservée entre deux étages
  (contre-épreuve : le même aller-retour par `f32` perd l'écart) ;
- conformité au schéma JSON et refus de documents invalides par le schéma.
