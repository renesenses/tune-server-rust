# #4346 — codec radio et conteneur de sortie

JP Robbe / OpenAI Codex / jp-robbe-20260917-4346-radio-source

## Périmètre

Base : `73707a08c1658289913058a9843250622be08521`.
Branche : `fix/jp-robbe-20260917-4346-radio-source`.
Lot : `batch/jp-p2-radio-20260917`.
Worktree Shrek : `/srv/builds/worktrees/jp-4346-20260917`.
Target dédié : `/srv/cache/tune/targets/jp-4346-20260917`.
Six jobs ; environnement `/srv/cache/tune/env.sh`. Aucun développement,
build ou test Rust sur le Mac.

La sonde Symphonia identifie le codec en amont du décodage radio. Auparavant,
seule la résolution PCM de sortie rejoignait le chemin du signal :
`NowPlaying.format = wav` devenait une source « WAV », donc sans perte,
même pour une station MP3.

La session porte maintenant une observation séparée : codec, fréquence et
profondeur source si connues. Le snapshot `stream_output_wire` conserve
le vrai conteneur de sortie WAV et transmet aussi cette observation.
Aucune modification du décodage audio, de la quantification ou du mixeur.

MP3, AAC, FLAC, ALAC, Vorbis et Opus sont identifiés depuis le codec de la
sonde, sans déduction à partir du titre de station ou du Content-Type.
Les codecs non mappés et le démarrage avant sonde restent « Unknown » et
ne valent jamais une preuve de source sans perte. La profondeur PCM 16 bits
du WAV n'est plus attribuée à la source MP3/AAC.

Le FLAC reste une source sans perte. Une réduction 24 → 16 bits dans le
décodeur radio ou une adaptation de fréquence empêche cependant le verdict
bit-perfect, y compris quand la sortie locale n'observe que le PCM déjà
converti. Les radios relayées directement conservent leur chemin existant.

## Validation exécutée sur Shrek

Préfixe commun :

```sh
export TUNE_TARGET_KEY=jp-4346-20260917 CARGO_BUILD_JOBS=6
. /srv/cache/tune/env.sh
```

- `cargo test -p tune-core --lib --no-default-features --features oaat radio_4346 -- --nocapture` :
  2 réussites. Vrais fichiers MP3 et FLAC 24/96 existants, servis en HTTP
  sur loopback, sonde et création du décodeur de production, publication puis
  lecture du snapshot de session. Ce banc s'arrête avant l'émission PCM.
- `cargo test -p tune-server --lib --no-default-features --features oaat signal_path_tests -- --nocapture` :
  68 réussites au premier passage. Les nouveaux contrats utilisent le
  constructeur JSON réel, sur sorties locale, OAAT et DLNA ; MP3, AAC,
  FLAC, codec inconnu, démarrage sans sonde, proxy et troncature 24 → 16.
  Le témoin #2427 conserve l'exigence de fréquence détectée mais attend
  maintenant MP3 48 kHz, au lieu du conteneur WAV.

### Contre-épreuve

Script et journaux : `/srv/builds/jp-evidence/jp-4346-20260917/`.

1. Retrait de `publish_radio_source(sonde.source_info)` dans le code de
   production, tests inchangés. Même commande core `radio_4346` :
   **2 échecs comportementaux**, sortie 101.
   - `radio_4346_mp3_probe_keeps_source_distinct_from_wav_output` :
     `MP3 station codec must survive decoding to WAV`, obtenu `None`,
     attendu `Some("mp3")`.
   - `radio_4346_flac_probe_keeps_original_resolution` : source vide au lieu
     de FLAC 24/96, message `source must retain 24-bit FLAC even though the
     radio output is 16-bit WAV`.
2. Restauration du producteur par `cp`. Désactivation du branchement de
   l'observation source dans `decrire_la_source`, tests inchangés.
   `cargo test -p tune-server --lib --no-default-features --features oaat radio_4346 -- --nocapture` :
   **3 échecs comportementaux, 1 témoin proxy vert**, sortie 101.
   - `radio_4346_signal_path_preserves_source_codec_and_output_container` :
     `radio source codec, not WAV`, obtenu `WAV 44kHz/16bit`, attendu `MP3 44kHz`.
   - `radio_4346_without_probe_cannot_claim_lossless` :
     `pending radio codec must not be inferred from WAV`, obtenu true.
   - `radio_4346_flac_truncated_before_local_output_is_not_bit_perfect` :
     `local: 24-bit source truncated to 16-bit before output`, obtenu true.
3. Restauration des deux fichiers par `cp` ; les deux SHA-256 concordent
   avec les sauvegardes (`restored-hashes.txt`). Aucune modification des
   témoins durant les contre-épreuves.

### Retour au vert après restauration

Même environnement et mêmes options Cargo :

| Commande (suffixe après `cargo test`) | Résultat |
| --- | --- |
| `-p tune-core --lib --no-default-features --features oaat radio_ -- --nocapture` | 121 réussites |
| `-p tune-core --lib --no-default-features --features oaat http::streamer::tests -- --nocapture` | 43 réussites |
| `-p tune-server --lib --no-default-features --features oaat signal_path_tests -- --nocapture` | 68 réussites |

Les filtres radio et streamer se recoupent : ne pas présenter leur somme
comme un nombre de tests distincts. La suite radio inclut les témoins de
décodage PCM et de reconnexion déjà présents dans le dépôt.

`cargo fmt --all -- --check` réussit.
`cargo clippy -p tune-core -p tune-server --lib --no-default-features --features oaat -- -D clippy::correctness`
réussit. Cette porte ne signifie pas zéro avertissement : 433 avertissements
core et 351 serveur sont rapportés sur ce graphe. Aucun avertissement ne
vise les lignes de production ajoutées par ce correctif.



## Limites et revue

- Pas de validation Windows/WASAPI ni sur le matériel du testeur sur Shrek.
- Le trajet exact du groupe 2 des captures (FLAC → FLAC, mixeur 192 kHz)
  n'est pas établi : ni explication inventée ni correctif du mixeur.
- Pas de mesure du débit des stations : une valeur absente reste absente.
- Aucun appel à une station publique ou à un compte de streaming.
- Le constructeur JSON est testé directement ; l'acceptation visuelle du
  client web et la lecture prolongée restent à vérifier.
- Le changement traverse la sonde, la session et le serveur : `ci:full`
  demandé pour la PR. La CI distante est distincte des preuves Shrek.
- Issue et verrou conservés pour la revue de Bertrand ; pas de merge,
  bump, tag ni déploiement.
