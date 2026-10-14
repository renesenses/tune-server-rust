# Étude #6357 — lire les radios HLS, en Rust pur, sans ffmpeg

*Étude seule, aucun code de production. Base lue : `batch/bugs-rc5-20261012`
(`995d7f898`). Date : 14/10/2026.*

## 0. Résumé

- **Le transport n'est pas le problème** : listes, segments et démultiplexage
  MPEG-TS sont du travail simple, faisable sans dépendance lourde.
- **Le codec, si** : le décodeur AAC de symphonia 0.6.0 (celui du `Cargo.lock`)
  ne fait **que l'AAC-LC**. Un flux HE-AAC y passe, mais sans SBR : moitié de
  la fréquence, bande coupée. La BBC hors Royaume-Uni sert du `mp4a.40.5`
  (HE-AAC).
- **Une crate en pur Rust décode l'HE-AAC v1 et v2** : `rusty_aac` 1.1.0
  (Apache-2.0, zéro dépendance). Elle est jeune (créée en juillet 2026), mais
  l'essai réel sur BBC Radio 3 la montre correcte (§3) : 48 kHz et HF
  reconstruites, là où symphonia sort du 24 kHz sans rien au-dessus de 11 kHz.
- Proposition : un `HlsSource` qui rend un flux ADTS continu, branché sur le
  `decode_radio_stream_to_pcm` actuel, avec un décodeur AAC `rusty_aac` placé
  dans le registre de symphonia. Environ 1 500 à 2 200 lignes, tests compris,
  en 4 PR.

## 1. Le code d'aujourd'hui

### 1.1 Le refus

- `tune-core/src/orchestrator/radio.rs` : `RADIO_HLS_UNSUPPORTED` (l. 167),
  `is_hls_manifest` (l. 200) : extension `.m3u8`, ou type
  `application/vnd.apple.mpegurl`.
- Deux portes dans le décodeur :
  - avant le réseau, sur l'URL (`decode_radio_stream_to_pcm_avec_profondeur`,
    l. 413) ;
  - après le GET, sur le `Content-Type` (`sonder_la_station`, l. 566).
- `commun.rs` (`resolve_playlist_url`, l. 265-300) ne déréférence que `.m3u` et
  `.pls`. Le `.m3u8` est volontairement écarté : son commentaire dit déjà que
  Tune n'a pas de chargeur de segments.
- `resolve_direct.rs` (l. 1097) range `RADIO_HLS_UNSUPPORTED` parmi les échecs
  **définitifs** (#3756) : la relance automatique du sondeur est désarmée.
- La migration 86 a retiré BBC Radio 3 pour cette raison
  (`db/migrations.rs`, l. 1262).

### 1.2 Le lecteur

`decode_radio_stream_to_pcm` (radio.rs l. 358) ne fait qu'appeler
`…_avec_profondeur`. C'est une boucle `'reconnect` sur `une_connexion` :

1. **`sonder_la_station`** fait le GET bloquant (`reqwest::blocking`, sans
   délai total, connexion en 10 s). Il contrôle le type, puis
   `ReadOnlySource::new(response)` → `MediaSourceStream` → sonde symphonia,
   guidée par l'extension (repli : `mp3`). Il construit ensuite le décodeur par
   `symphonia::default::get_codecs()`.
2. **`preparer_la_sortie`** fixe le format de sortie **à partir des
   paramètres du codec**, avant tout décodage (`source_sample_rate`,
   `source_channels`). Il mémorise `expected_format` : une reconnexion qui
   change de format met fin à la session (`radio_reconnect_format_changed_bailing`).
3. **`decoder_un_paquet`**, en boucle : `AmontTermine`, `AmontCoupe` et
   `ErreurDePaquet` mènent à la reprise.
4. **`reprendre_apres_coupure`** : au plus `MAX_RECONNECTS = 30` reprises,
   compteur remis à zéro après une bonne période, 500 ms entre deux essais.

Deux appelants :
- **chemin local et OAAT** : `decoder_la_radio_en_wav`
  (`resolve_direct.rs` l. 978, `sortie_locale = true` pour la sortie locale) ;
- **chemin réseau** : le proxy WAV vers un renderer (`needs_proxy`, l. 1781 et
  suivantes). Le passage direct de l'URL au renderer n'est pris que pour une
  extension « fiable » (`.mp3`, `.flac`, `.wav`), ce que n'est pas le `.m3u8`.
  Un flux HLS passe donc **toujours** par le décodeur de Tune, et c'est voulu :
  aucun renderer UPnP ne garantit HLS.

Bandcamp vers OAAT (`resolve_direct.rs` l. 1193) réutilise le même décodeur.
Il n'est pas concerné.

### 1.3 Métadonnées ICY

Le décodeur ne demande pas `Icy-MetaData` : les titres viennent d'un sondeur à
part, `radio_metadata.rs` (`fetch_icy_metadata`, l. 1117, avec sa propre
connexion), et des fournisseurs « now playing ». Pour le HLS, il n'y a pas
d'ICY. Le titre doit venir de l'ID3 des segments ou du fournisseur (voir §4.6).

### 1.4 Le contrôle à l'enregistrement

`tune-server` `routes::radios::sonder_le_flux` (l. 271, appelé par
`create_radio`/`update_radio`) juge avec `non_audio_content_type`. Il faudra
qu'il accepte une liste HLS valide, et refuse une liste dont aucune variante
n'est décodable.

## 2. Symphonia 0.6.0 (version du `Cargo.lock`)

Sources lues : `~/.cargo/registry/src/index.crates.io-*/symphonia-*-0.6.0`,
identiques sur Shrek. Fonctions activées dans le `Cargo.toml` de l'espace de
travail : `mp3, flac, wav, aac, alac, vorbis, pcm, isomp4, ogg, mkv`.

| Question | Réponse | Preuve |
|---|---|---|
| MPEG-TS | **Non.** Aucun lecteur TS dans 0.6.0 ni 0.6.1, et aucune crate `symphonia-format-*ts`. | `symphonia-0.6.0/Cargo.toml` `[features]` : `caf, isomp4, mkv, ogg, aiff, wav` seulement |
| fMP4 / CMAF | **Oui, en principe.** `isomp4` lit `moof`+`mdat` et, sur une source non adressable, avance fragment après fragment. Non essayé (§3). | `symphonia-format-isomp4-0.6.0/src/demuxer.rs` l. 146, 211-220, 268, 278, 627 |
| ADTS brut | **Oui.** `AdtsReader`, dans `symphonia-codec-aac` (fonction `aac`), une trame par paquet. | `symphonia-codec-aac-0.6.0/src/adts.rs` l. 39, 190 |
| AAC-LC | Oui. | `aac/mod.rs` l. 40 : « AAC-LC » |
| HE-AAC (SBR) | **Non.** | voir ci-dessous |
| HE-AAC v2 (PS) | **Non.** | idem |

Le refus de l'HE-AAC dépend de la façon dont il est signalé :

- **Signalement explicite** (MP4/fMP4, `esds` en objet 5 ou 29) :
  `AudioSpecificConfig::read` remplace l'objet par l'objet cœur (LC) et pose
  `sbr_present = true` (`symphonia-common-0.6.0/src/mpeg/audio/mod.rs`
  l. 246-261). `AacDecoder::try_new` refuse alors, avec « aac: aac too
  complex » (`symphonia-codec-aac-0.6.0/src/aac/mod.rs` l. 91-96). Le flux ne
  se lit **pas du tout**.
- **Signalement implicite** (ADTS, cas des segments TS) : l'en-tête ADTS dit
  « LC » à la fréquence du cœur. Le décodeur s'ouvre. Il voit passer les
  éléments `ID_FIL` de type `EXT_SBR_DATA` et note `sbr_present`, mais il
  **saute** leur contenu (`aac/mod.rs` l. 186-200, « Ignore extension
  payload(s) »).
- Résultat de ce second cas : un PCM à **la moitié de la fréquence**
  (24 kHz au lieu de 48 kHz). La bande s'arrête au bord haut du cœur LC,
  c'est-à-dire à la fréquence de raccord du SBR. Pas de HF, et le PS est
  perdu (un HE-AAC v2 sort en mono). Cela s'entend nettement, mais ne produit
  aucune erreur. Mesure au §3.
- Le profil annoncé est `CODEC_PROFILE_AAC_LC` seulement (`aac/mod.rs`
  l. 295-301).
- 0.6.1 est identique sur ce point : seul un `unwrap` change en `expect`.

**Ce qui touche déjà Tune** : toute station Icecast en `audio/aacp`
(HE-AAC en ADTS) passe aujourd'hui par ce chemin dégradé, sans rien dire. Le
décodeur HE-AAC proposé ici l'améliorerait aussi.

## 3. Essai réel (Shrek, 14/10/2026)

Dossier jetable `/srv/builds/bertrand/hls-6357-b209c13`, purgé à la fin.
Total téléchargé : environ 250 Ko (3 listes et 3 segments de 81 592 octets).

### 3.1 Les listes

Liste maîtresse (`a.files.bbci.co.uk/…/nonuk/pc_hd_abr_v2/aks/bbc_radio_three.m3u8`) :
`200`, `content-type: application/x-mpegurl` (pas le type enregistré),
`cache-control: max-age=300`. Elle contient **une seule** variante :

```
#EXT-X-STREAM-INF:PROGRAM-ID=1,BANDWIDTH=101760,CODECS="mp4a.40.5"
https://as-hls-ww-live.akamaized.net/pool_23461179/live/ww/bbc_radio_three/bbc_radio_three.isml/bbc_radio_three-audio%3d96000.norewind.m3u8
```

La liste de variante (Unified Streaming 1.13.5) répond en
`application/vnd.apple.mpegurl`, `cache-control: max-age=2` :

```
#EXT-X-VERSION:3
#EXT-X-MEDIA-SEQUENCE:279994985
#EXT-X-INDEPENDENT-SEGMENTS
#EXT-X-TARGETDURATION:6
#USP-X-TIMESTAMP-MAP:MPEGTS=1089719200,LOCAL=2026-10-14T08:51:37.600000Z
#EXT-X-PROGRAM-DATE-TIME:2026-10-14T08:51:37.600000Z
#EXTINF:6.4, no desc
bbc_radio_three-audio=96000-279994985.ts
… (5 segments : fenêtre glissante de 32 s, sans #EXT-X-ENDLIST, sans clé)
```

Les segments sont en chemins relatifs, numérotés comme la séquence. Ils
pèsent 81 592 octets (434 paquets TS), en `video/MP2T`.

### 3.2 Résultats

Programme jetable (non versionné) : `symphonia =0.6.0` (fonction `aac`
seule), `rusty_aac =1.1.0`, `m3u8-rs =6.0.1`, et un démultiplexeur TS écrit
à la main (environ 120 lignes). Il prend les 3 segments **consécutifs** et les
concatène, puis compare les deux décodeurs. Pour mesurer l'énergie par bande,
il applique un Goertzel sur des fenêtres de Hann de 2 048 points (en dB, 0 =
bande la plus forte). Compilé en `--release` sur Shrek, sous jeton `flock -o`.

```
m3u8-rs maîtresse : bw=101760 codecs=Some("mp4a.40.5") uri=https://as-hls-ww-live…/bbc_radio_three-audio%3d96000.norewind.m3u8
m3u8-rs média     : seq=279994985 target=6 segments=5 endlist=false pdt0=Some(2026-10-14T08:51:37.600+00:00) discontinuités=0
TS: 1302 paquets, PMT Some(32), flux (type,pid) [(f, 22)], erreurs CC 0, PES audio 75, ADTS 233536 octets, ID3 0
  pas des PTS (90 kHz) distincts: [23040]
  borne de segment: PTS 1091424160 -> 1091447200 (écart 23040)
  borne de segment: PTS 1092000160 -> 1092023200 (écart 23040)
ADTS: 450 trames, (objet, indice de fréquence, config canaux) = Some((2, 6, 2)), trames à plusieurs blocs 0, reste 0 o
symphonia: format=aac codec=AudioCodecId(4103) profil=None rate=Some(24000) canaux=Some(2)
symphonia: 450 paquets, 460800 trames PCM à 24000 Hz x2 = 19.20 s, 0 erreurs
  symphonia bandes dB: 1k:0 2k:-20 3k:-35 4k:-40 5k:-49 6k:-53 7k:-55 8k:-56 9k:-57 10k:-58 11k:-66
rusty_aac: 921600 trames PCM à 48000 Hz x2 = 19.20 s, 0 erreurs, sbr_support=NotPresent, décodé en 85.115001ms
  rusty_aac bandes dB: 1k:0 2k:-18 3k:-35 4k:-36 5k:-47 6k:-51 7k:-53 8k:-54 9k:-56 10k:-57 11k:-68 12k:-58 13k:-59 14k:-60 15k:-60 16k:-63 17k:-60 18k:-61 19k:-60 20k:-66 21k:-66 22k:-83 23k:-103
```

Ce qui marche :

- **Les listes** : `m3u8-rs` lit les deux listes BBC, avec la variante, le
  `CODECS`, la séquence, la cible, `PROGRAM-DATE-TIME`, l'absence de clé et
  l'absence d'`ENDLIST`.
- **Le TS** :
  - un PAT, un PMT (PID 32) et un seul flux, de type `0x0F` (ADTS AAC), sur
    le PID `0x22` ;
  - **pas d'ID3** dans les TS de la BBC ;
  - **aucune erreur de continuité, ni dans un segment ni d'un segment à
    l'autre** ;
  - le pas des PTS est constant (23 040 ticks = 256 ms = 6 trames ADTS par
    PES), y compris aux deux bornes de segment.
  - Conclusion : des segments consécutifs se **concatènent sans trou**. Un
    `HlsSource` qui rend « les ADTS des segments, bout à bout » donne un flux
    parfait.
- **L'ADTS** : 450 trames, toutes à un seul bloc. L'en-tête dit **objet 2
  (LC), indice 6 (24 000 Hz), stéréo**, alors que la liste annonce
  `mp4a.40.5`. C'est le signalement **implicite** de l'HE-AAC, le cas du §2.
- **symphonia 0.6.0** : il ouvre et décode **sans aucune erreur**, mais à
  **24 000 Hz**. Il n'y a rien au-dessus de 12 kHz (sa fréquence de Nyquist),
  et la bande s'éteint dès 11 kHz (−66 dB). La durée est juste (19,20 s) : la
  musique est à la bonne hauteur, mais **on perd la moitié haute du
  spectre** et le flux ne dit rien. Le profil est annoncé `None`, alors que
  la liste dit HE-AAC.
- **rusty_aac 1.1.0** : il décode **sans aucune erreur, à 48 000 Hz**, et la
  durée est identique. Sous 11 kHz, les bandes sont celles de symphonia, à
  2 dB près : le cœur LC est le même. **De 12 à 21 kHz, il y a de
  l'énergie** (−58 à −66 dB), là où symphonia n'a rien : le SBR est
  reconstruit. L'énergie tombe au-delà de 22 kHz.
  - Il décode 19,2 s en 85 ms, soit environ 225 fois le temps réel sur un
    cœur de Shrek.
  - `sbr_support()` dit `NotPresent`, parce qu'il ne regarde que la
    configuration fournie en octets (le cas MP4). En ADTS implicite, il faut
    lire la fréquence **de la trame décodée**, et non cette méthode : c'est le
    piège du §4.4.

**Géorestriction, vérifiée depuis Shrek (Allemagne)** :

- La liste maîtresse **`uk`** (même chemin, `nonuk` → `uk`) répond `200`. Elle
  annonce **une autre** variante, à 320 kbit/s en **AAC-LC** (`mp4a.40.2`,
  hôte `as-hls-uk-live`).
- Cette variante répond **`403`**.
- Conclusion :
  - le blocage est sur la **variante**, pas sur la liste maîtresse. La
    détection doit donc se faire au premier GET de variante ou de segment ;
  - au Royaume-Uni, la BBC sert de l'AAC-LC à 320k, que symphonia décode déjà
    seul. Hors du Royaume-Uni, elle sert de l'HE-AAC à 96k, ce qui exige le
    nouveau décodeur.

### 3.3 Ce qui n'a pas été essayé

- fMP4/CMAF (`EXT-X-MAP`) : la BBC n'en sert pas sur cette liste.
- AES-128 : aucune clé sur la liste BBC.
- Une lecture longue avec relecture de la liste (fenêtre qui glisse) : c'est
  la première tâche de l'étape 1 (§5).

## 4. Architecture proposée

```
URL .m3u8 ─▶ HlsSource (fil de chargement) ──octets ADTS──▶ ReadOnlySource
               │ liste maîtresse → variante                     │
               │ relecture de la liste, fenêtre, tampon         ▼
               │ segments TS ─▶ démultiplexeur TS ─▶ ADTS   MediaSourceStream
               │                         └─▶ ID3 (titres)       │
               ▼                                                ▼
        erreurs passagères absorbées            AdtsReader (symphonia, inchangé)
        erreur durable → Read::Err                              │
                                                                ▼
                                          décodeur AAC rusty_aac, enregistré
                                          dans un CodecRegistry propre à Tune
                                                                │
                                                                ▼
                                   preparer_la_sortie / decoder_un_paquet (inchangés)
```

### 4.1 `HlsSource` : un `Read` comme un flux HTTP continu

- Nouveau module `tune-core/src/radio_hls/` (`liste.rs`, `source.rs`,
  `ts.rs`, `id3.rs`). Le décodeur n'y voit qu'un `impl Read + Send`, posé dans
  `ReadOnlySource` comme la réponse `reqwest` d'aujourd'hui. Pour symphonia, c'est
  un flux ADTS infini et non adressable, comme une station Icecast `.aac`.
- **Branchement** : dans `sonder_la_station`, si `is_hls_manifest(url, ct)`,
  on construit un `HlsSource` au lieu de refuser, avec l'indice `aac`. Les deux
  `return Err(RADIO_HLS_UNSUPPORTED…)` deviennent ce branchement.
  `RADIO_HLS_UNSUPPORTED` reste pour ce que Tune ne lit toujours pas : liste
  chiffrée autrement qu'en AES-128, codec hors AAC, fMP4 s'il est repoussé.
- **Fil de chargement** : un fil (`std::thread`, `reqwest::blocking`, comme le
  reste du décodeur radio) alimente un canal borné de blocs ADTS. `Read` vide le
  canal, et bloque quand il est vide.
- **Choix de la variante** : on garde les variantes dont `CODECS` est décodable
  (`mp4a.40.2`, `.5`, `.29`, `mp4a.40.34`/MP3 si présent), puis la plus haute
  `BANDWIDTH`. À égalité, on préfère `AVERAGE-BANDWIDTH`. Une variante audio
  seule est préférée à une variante vidéo. On garde **une** variante pour
  toute la session : pas d'ABR, une radio n'en a pas besoin, et changer de
  variante change le format. On ne passe à une variante plus basse qu'après
  des échecs répétés (décision §6).
- **Démarrage** (RFC 8216 §6.3.3) : au 3ᵉ segment avant la fin. Sur BBC, cela
  fait environ 19 s de retard sur le direct et 3 segments d'avance en tampon.
- **Relecture de la liste** : toutes les `EXT-X-TARGETDURATION` secondes. On
  attend la moitié seulement si la liste n'a pas bougé (§6.3.4). On suit
  `EXT-X-MEDIA-SEQUENCE` : un segment déjà joué n'est jamais rechargé, et un
  segment sorti de la fenêtre avant d'être chargé est un **trou**, journalisé
  (`hls_segment_perdu`), après quoi on reprend au plus ancien disponible.
- **Tampon d'avance** : de 2 à 3 segments téléchargés d'avance. Le canal est
  borné en octets, environ 3 × 82 Ko pour la BBC.
- **Reprise sur erreur**, en deux niveaux :
  - *Passagère* (délai, 5xx, 404 sur un segment) : elle est absorbée dans le
    fil, par un nouvel essai avec attente croissante. Un segment perdu est
    sauté. Symphonia n'en voit rien.
  - *Durable* : la liste échoue N fois de suite (N ≈ 5 ou environ 30 s), ou
    elle se termine (`#EXT-X-ENDLIST`). `Read` rend alors une erreur ou EOF, et
    la boucle `'reconnect` actuelle prend la main (`AmontCoupe`/`AmontTermine`),
    avec un nouvel `HlsSource` et une liste fraîche. `MAX_RECONNECTS` et
    `expected_format` gardent leur rôle.
- Les URL relatives se résolvent par `url::Url::join`, contre l'URL **finale**
  après redirection. `url` est déjà dans le `Cargo.lock`.

### 4.2 Les listes : `m3u8-rs`

`m3u8-rs` 6.0.1 (MIT, 1,66 M téléchargements, mise à jour le 30/07/2026). Ses
dépendances sont `nom 7` et `chrono 0.4`, toutes deux **déjà** dans notre
`Cargo.lock` : elle n'ajoute aucune crate. Elle a lu les deux listes BBC
(§3.2). `hls_m3u8` 0.7.0 (MIT/Apache, plus stricte) est l'autre choix
sérieux. Écrire notre propre lecteur (environ 200 lignes) se défend aussi :
on n'utilise que 8 balises.

### 4.3 Démultiplexage TS → ADTS

- Un TS audio HLS ne contient qu'un PAT, un PMT, un PID audio (type `0x0F`,
  ADTS) et parfois un PID ID3 (type `0x15`). Le démultiplexeur jetable du §3 fait
  cela en 120 lignes : synchro `0x47`, champ d'adaptation, PAT/PMT, PES, PTS,
  compteur de continuité.
- Proposition : **l'écrire chez nous**, environ 300 lignes avec les tests.
  On y ajoute la reprise de synchro sur un `0x47` perdu, le PES qui court sur
  plusieurs paquets et un PMT qui change.
- Crates possibles si on préfère :
  - `mpeg2ts` 0.6.1 (MIT, zéro dépendance, active) ;
  - `mpeg2ts-reader` 0.18.2 (MIT/Apache, plus lourde, dernière version en
    janvier 2025) ;
  - `oxideav-mpegts` (trop jeune).
- **Sortie : de l'ADTS**, pas des paquets. Elle passe par l'`AdtsReader` de
  symphonia sans rien changer en aval.
- La variante « flux de paquets » (un `FormatReader` HLS propre) garderait
  les PTS et les bornes de segment. Elle oblige à réécrire la sonde, ce qui
  ne vaut que si l'on veut un jour se caler sur `PROGRAM-DATE-TIME`.
- Autres charges utiles : **MP3 en TS** (types `0x03`/`0x04`), à garder car
  les octets MP3 passent tels quels. **AC-3 et E-AC-3** (`0x81`) : refus
  nommé. **LATM** (`0x11`) : rare en HLS, refus. **Audio brut**
  (`.aac`/`.mp3` en segments, avec un ID3 PRIV
  `com.apple.streaming.transportStreamTimestamp` en tête) : on retire l'ID3,
  on passe le reste.

### 4.4 Le décodeur

On a le choix entre trois voies.

| Voie | Pur Rust | Licence | État | Verdict |
|---|---|---|---|---|
| `symphonia-codec-aac` 0.6 | oui | MPL-2.0 | LC seulement | insuffisant (§2) |
| **`rusty_aac` 1.1.0** | **oui**, zéro dépendance, 36 `unsafe` (SIMD, inventoriés dans `UNSAFE.md`), et zéro sans SIMD | **Apache-2.0** | créée le 29/07/2026, 7 versions, environ 10 000 téléchargements. Annonce LC, Main, LTP, HE-AAC v1/v2 et LD/ELD, avec 83 flux de la suite ISO 14496-26 décodés à 1 LSB près de FFmpeg 8.1.2 | **recommandé**, à surveiller |
| `syom` 0.7.1 | oui | MIT | créée en 09/2026, 104 téléchargements | trop jeune |
| `fdk-aac` 0.8 / `fdk-aac-sys` 0.5, ou `symphonia-adapter-fdk-aac` 0.2 | **non**, C (libfdk-aac compilée par `cc`) | crate MIT, mais **le code C est sous la « Fraunhofer FDK AAC license »** : la FSF la juge incompatible avec la GPL, elle ne donne aucun droit de brevet, et Debian range le paquet en `non-free` | mûr | repli seulement, **à signaler** |
| `faad2` / `faad2-sys` 0.1.0 | non, C | **GPL-2.0** | abandonné depuis 2020 | **exclu** (GPL contaminante) |
| `redlux` 0.7 | non (repose sur fdk-aac) | MIT + FDK | 2023 | exclu |

Points de vigilance sur `rusty_aac` :

- **Brevets** : son README dit que la licence ne donne aucun droit de brevet
  sur le SBR et le PS. C'est vrai de tout décodeur HE-AAC, symphonia compris
  s'il le faisait un jour. La question se pose pour les binaires distribués, à
  savoir les apps et l'image Tune OS (décision §6).
- **Jeunesse** : un seul mainteneur (Remade-With-Rust / Mata Network), 2 mois
  et demi d'existence. Parade : épingler la version avec `=1.1.0`, et la
  **vendoriser** au besoin, comme `vendor/rubato`.
- **Intégration** : on écrit un adaptateur `symphonia::core::codecs::audio::AudioDecoder`
  d'environ 150 lignes, sur le modèle de `symphonia-adapter-fdk-aac`. On
  l'enregistre dans un `CodecRegistry` à nous, qui garde les décodeurs de
  symphonia sauf l'AAC. `sonder_la_station` y passe au lieu de
  `symphonia::default::get_codecs()`.
- **Piège du format de sortie** : `preparer_la_sortie` lit la fréquence et
  les canaux dans les **paramètres** du codec, **avant** de décoder. Pour un
  HE-AAC implicite (ADTS), ces paramètres disent 24 kHz. L'adaptateur doit
  donc décoder la première trame dès la sonde, puis annoncer la vraie
  fréquence (48 kHz) et les vrais canaux (stéréo si PS). Une autre manière
  est de le déduire de `CODECS="mp4a.40.5"`/`.29` dans la liste maîtresse,
  mais elle ne marche pas pour l'Icecast `aacp`. Si on l'oublie, on envoie un
  en-tête WAV à 24 kHz sur du PCM à 48 kHz : la musique joue à la moitié de la
  vitesse.
- **Hors HLS** : faut-il prendre ce décodeur pour **tout** l'AAC (Icecast
  `aacp`, fichiers `.m4a` HE-AAC de la bibliothèque), ou pour le HLS seulement ?
  Décision §6.

### 4.5 Cas particuliers

- **`#EXT-X-DISCONTINUITY`** (pub, raccord de programme, redémarrage du
  codeur) :
  - En ADTS, le flux reste lisible. On vide le tampon de bits du décodeur, et
    `rusty_aac` se reconfigure seul quand l'en-tête ADTS change.
  - Si la **fréquence ou les canaux** changent, le garde-fou `expected_format`
    met déjà fin à la session. Un nouveau `HlsSource` repart alors, avec un
    nouvel en-tête WAV, et l'auditeur entend une courte coupure.
  - On ignore les PTS qui repartent de zéro, puisqu'on ne s'en sert pas pour
    caler.
  - `EXT-X-DISCONTINUITY-SEQUENCE` sert à garder le compte juste à travers les
    relectures.
- **Chiffrement** :
  - `METHOD=AES-128` (segment entier en AES-128-CBC, clé par URI, IV donné ou
    numéro de séquence) : **à inclure**. C'est de l'**obscurcissement de
    transport**, pas des DRM, la clé est servie en clair à tout client. `aes`
    0.8 et `cbc` 0.1 sont **déjà** dans le `Cargo.lock`. Il faut environ 80
    lignes et un cache de la clé par URI.
  - `SAMPLE-AES`, `SAMPLE-AES-CTR` et toute `KEYFORMAT` FairPlay ou Widevine,
    c'est-à-dire des DRM : **exclus**, avec un refus nommé
    (`RADIO_HLS_UNSUPPORTED: chiffrement DRM`). On ne contourne aucune DRM.
- **Métadonnées** :
  - *ID3 dans le TS* (PID `0x15`, ou ID3 en tête des segments bruts) : on lit
    `TIT2`/`TPE1`, ou `TXXX`/`PRIV` selon le diffuseur. Le titre remonte par
    le même chemin que le « now playing » de `radio_metadata.rs` (voir §6 pour
    l'ordre de priorité).
  - *`EXT-X-PROGRAM-DATE-TIME`* : l'heure murale du segment. Elle peut caler
    un « now playing » horodaté (grille de la BBC, fiches radio-metadata) sur ce qui
    sort vraiment, avec le retard d'environ 19 s du tampon. C'est une
    amélioration pour plus tard : l'étape 1 ne fait que la lire et la
    journaliser.
- **Flux restreints par pays** :
  - La BBC sépare les listes `uk` et `nonuk`. Hors du Royaume-Uni, la liste
    maîtresse `uk` répond `200`, mais sa variante répond `403` (§3.2). Une
    station livrée doit donc pointer sur la liste **`nonuk`**, comme celle de
    l'essai. Un auditeur au Royaume-Uni peut ajouter la liste `uk`, qui est
    meilleure (AAC-LC à 320k).
  - Un `403` sur la liste ou sur les segments devient un échec **définitif**
    nommé (`radio_geo_bloquee`, nouveau préfixe à côté de `RADIO_NOT_AUDIO`).
    On ne fait pas 30 essais.
  - **Aucun contournement** : ni proxy, ni en-tête falsifié.
- **Enregistrement d'une station** : `sonder_le_flux` (côté `tune-server`)
  doit lire la liste maîtresse, trouver une variante décodable et un premier
  segment `200`, puis rendre le codec (`aac-he`) au lieu de `text/x-mpegurl`.

### 4.6 Ce qui reste hors du périmètre

- La lecture différée ou la remontée dans la fenêtre (`DVR`). Une radio en
  direct n'en a pas besoin.
- LL-HLS (`EXT-X-PART`, rechargement bloquant). Les balises sont ignorées
  sans dommage par un client HLS classique.
- `EXT-X-MEDIA` : pistes audio alternatives ou langues. On prend la piste par
  défaut.

## 5. Étapes et taille

| # | PR | Contenu | Taille estimée (avec tests) |
|---|---|---|---|
| 1 | `radio_hls` : listes + `HlsSource` + TS→ADTS | m3u8-rs, choix de variante, fenêtre, relecture, tampon, reprise à deux niveaux, AES-128, démultiplexeur TS, ID3 lu et journalisé. Tests sur un serveur `axum` local qui sert une fenêtre glissante factice, avec segments TS **générés** dans le test (pas d'enregistrement de la BBC dans le dépôt, pour les droits) | 900 à 1 200 lignes |
| 2 | Décodeur `rusty_aac` | adaptateur `AudioDecoder`, registre Tune, annonce de la vraie fréquence à la sonde, témoin HE-AAC (fréquence ×2 et énergie au-dessus de 12 kHz). Contre-épreuve : avec le registre par défaut, le témoin rougit sur « 24000 » | 250 à 400 lignes |
| 3 | Branchement | `sonder_la_station` (deux portes), `sonder_le_flux`, refus nommés (`radio_geo_bloquee`, DRM), traces. Témoins du chemin local et du chemin réseau (`decode_radio_temoins.rs`). Mise à jour des commentaires de `commun.rs` et `radio.rs`, qui disent aujourd'hui « Tune ne lit pas HLS » | 200 à 350 lignes |
| 4 | Données | migration (premier numéro libre, voir les règles) qui remet BBC Radio 3 et les autres stations HLS de la base radio-metadata (82 stations), en URL `nonuk` | 100 à 200 lignes |

Total : environ **1 500 à 2 200 lignes**. Il faut 3 à 4 jours de chantier,
plus une écoute réelle sur le .18 (sortie locale) et sur un renderer UPnP
(proxy WAV). fMP4/CMAF n'est pas compté : environ 150 lignes de plus si on le
décide (§6).

### Risques

- **`rusty_aac` jeune** : une régression ou un abandon. Parade : version
  épinglée, vendorisation possible, et repli sur symphonia (LC dégradé) si
  `rusty_aac` échoue à l'ouverture, avec une trace.
- **Le format décidé avant le décodage** (§4.4) : le piège le plus probable,
  qu'un témoin dédié doit couvrir.
- **Retard** : environ 19 s sur le direct, inhérent au HLS. Cela compte si un
  « now playing » horodaté est affiché sans calage.
- **Diffuseurs hétérogènes** : TARGETDURATION non respecté (BBC : EXTINF 6,4
  pour une cible de 6), listes sans `PROGRAM-DATE-TIME`, segments sans PAT
  au début. Il faut un démultiplexeur tolérant.
- **Charge** : un GET de liste toutes les 6 s et un segment toutes les
  6,4 s par station écoutée, environ 13 Ko/s. Le décodage HE-AAC coûte
  environ 0,5 % d'un cœur (§3.2).
- **Brevets HE-AAC** pour les binaires distribués (§4.4).

## 6. Décisions laissées à Bertrand

1. **Décodeur HE-AAC** : `rusty_aac` (pur Rust, jeune), ou `fdk-aac` (C,
   licence FDK non libre, mûr), ou attendre symphonia (rien d'annoncé dans
   0.6.1) ?
2. **Périmètre du nouveau décodeur** : HLS seulement, ou tout l'AAC de Tune
   (Icecast `aacp` aujourd'hui dégradé sans le dire, et fichiers HE-AAC de la
   bibliothèque) ?
3. **AES-128** : à inclure (recommandé, ce n'est pas une DRM), ou à exclure ?
4. **fMP4/CMAF** dès l'étape 1, ou plus tard, sur la preuve d'une station de
   la base qui en sert ?
5. **Variante** : toujours la plus haute, ou descendre d'une variante après
   des échecs répétés ?
6. **Métadonnées** : quand l'ID3 du flux et le fournisseur « now playing » se
   contredisent, lequel l'emporte ?
7. **Stations géobloquées** : faut-il les livrer, en `nonuk` seulement, et
   quel message afficher sur un `403` ?
8. **Brevets** : question juridique sur la distribution du SBR et du PS dans
   les apps et l'image Tune OS.
9. **Calendrier** : à prendre dans la rc5 ou après ? C'est une nouvelle
   fonction, qui demande son go.
