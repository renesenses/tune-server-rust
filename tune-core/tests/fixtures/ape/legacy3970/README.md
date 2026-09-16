# APE 3.97 — régression #4191

Ces huit fichiers contiennent uniquement des signaux synthétiques écrits pour
Tune. Aucun extrait musical ni fichier du testeur n'est versionné.

## Référence indépendante

`generate.py` définit le PCM **avant** encodage : ramps modulaires bornées,
deux canaux distincts, pseudo-stéréo et silence. Le SDK historique Monkey's
Audio 3.97 encode ce PCM, avec son CRC et ses prédicteurs. Les empreintes
`pcm_sha256` du manifeste proviennent de ces octets d'entrée, jamais du
décodeur corrigé. `tune_pcm_sha256` tient compte du seul élargissement 8 bits
non signé vers 16 bits signé effectué par Tune.

Le 2026-09-15 sur Shrek, les huit sorties ont également été décodées par
FFmpeg 8.0 puis comparées octet par octet au PCM d'entrée : égalité dans tous
les cas. Cet outil de référence intervient seulement pendant la préparation
des fixtures ; ni Tune ni la CI ne l'invoquent pour ces tests.

- SDK 3.97, port historique du mainteneur : https://tmkk.undo.jp/monkey/sdk.html
- Archive : https://tmkk.undo.jp/monkey/MAC_SDK_397_OSX_20040718.tar.gz
- SHA-256 : `ccc24b2fccb0eaf0dd3c8eb3ccad74b42d2bb26cd87c6b3fe0da7d0fd6ea8d83`
- Référence : https://ffmpeg.org/releases/ffmpeg-8.0.tar.xz
- SHA-256 : `b2751fccb6cc4c77708113cd78b561059b6fa904b24162fa0be2d60273d27b8e`

Le SDK reste externe, sous sa licence ; aucun de ses fichiers n'est livré.
`prepare_encoder.py` adapte uniquement son environnement de compilation :
DWORD de 32 bits, ordre des octets x86, suppression des macros min/max
incompatibles avec C++ moderne et désactivation d'AltiVec au profit du chemin
scalaire existant. `encode.cpp` est notre adaptateur mémoire de l'encodeur.
`generate.py` assemble l'en-tête ancien et sa table de recherche autour des
vraies trames encodées. La première trame du cas multi-trames contient bien
294 912 blocs, suivie de 4 096 blocs.

## Régénération

Sur Shrek, dans un répertoire temporaire **vide** (Python ≥ 3.12, g++) :

```sh
python3 /chemin/du/worktree/tune-core/tests/fixtures/ape/legacy3970/prepare_encoder.py
```

La sortie est dans `fixtures/`. Le script vérifie le SHA-256 du SDK avant
extraction. Tous les `*.ape` ont été régénérés à l'identique dans un second
répertoire ; le manifeste a aussi été comparé comme JSON.

Pour la vérification indépendante, avec un binaire de référence compilé
séparément et son chemin explicite :

```sh
/path/to/ffmpeg -v error -i fixtures/stereo16_c4000.ape   -acodec pcm_s16le -f s16le reference.pcm
cmp fixtures/stereo16_c4000.pcm reference.pcm
```

Pour 24 bits utiliser `pcm_s24le/s24le` ; pour 8 bits `pcm_u8/u8`.

## Couverture et limites

`integration_contracts::ape_legacy_4191` vérifie le PCM complet de chaque
fixture via la dépendance et l'entrée réelle de Tune, la lecture progressive,
un seek traversant une vraie limite de trame et le refus d'un CRC altéré.
Le fichier moderne existant `../sine_16s_c3000.ape` conserve son témoin WAV.

Le corpus cible 3970, en compression 2000 et surtout 4000, avec 8/16/24 bits,
mono/stéréo. Il ne prétend pas couvrir toutes les versions historiques,
tous les niveaux de compression ni le CDImage complet du testeur.

## Témoin moderne de même compression

Le fichier voisin `../sine_16s_c4000.ape` est le sinus synthétique 3990/4000
du projet amont (MIT OR Apache-2.0, licences conservées dans
`vendor/ape-decoder/`), extrait du commit immuable :
https://github.com/OMBS-IO/ape-decoder/blob/1f60c2db467964cf734b69d18041f6b911400556/tests/fixtures/ape/sine_16s_c4000.ape

SHA-256 APE : `de44af98cc0c5d2eb75671aa005542b914d96b61ff8faa60743f3d7704f620f4`.
Ses 176 400 octets PCM sont identiques au WAV de référence 3000 déjà versionné
et à la sortie FFmpeg indépendante (SHA-256
`cec05fa57320a6589e09e8dd2ee5230580129de4f42c95b8fb6b5041c4e4d853`).
Le même test garde donc 3990/3000 et 3990/4000.
