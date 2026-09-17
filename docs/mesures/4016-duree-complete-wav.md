# #4016 — le transport finit, le lecteur WAV peut finir avant lui

JP Robbe / OpenAI Codex / jp-robbe-20260917-4016-long-stream

Base mesurée : `3a2b710a151a257e217143b2900eeb53076ea6ef`.

Le correctif #4081 permet le démarrage du fichier hors plafond RIFF.
La confirmation du testeur porte sur ce démarrage. Le résidu de durée
complète se mesure sans son DAC : le transport HTTP et le conteneur WAV
annoncent deux tailles différentes.

## Mesure réalisée sur Shrek

Le programme `tune-stream-http/examples/mesure_long_wav_4016.rs` crée une
vraie session `StreamSession`, monte le routeur de production sur un port
loopback éphémère et consomme la réponse avec Reqwest. Son producteur fournit
6 359 040 000 octets de PCM synthétique, soit 46 minutes en stéréo 24 bits à
384 kHz. Le dernier bloc porte un marqueur contrôlé à réception.

Deux chemins sont exercés :

- l'en-tête est ajouté par la route HTTP avec la durée de la session ;
- le producteur fournit l'en-tête sans durée, comme le décodeur progressif,
  et la session le déclare par `wav_header_included`.

| Mesure | En-tête HTTP | En-tête du producteur |
|---|---:|---:|
| `Content-Length` | 6 359 040 044 | 6 359 040 044 |
| Octets effectivement reçus | 6 359 040 044 | 6 359 040 044 |
| Marqueur final reçu | oui | oui |
| Taille `data` de l'en-tête WAV reçu | 2 147 483 611 | 2 147 483 611 |
| Trames décrites par le WAV | 357 913 935 | 357 913 935 |
| Durée décrite à 384 kHz | 932,067539 s | 932,067539 s |
| Trames fournies par le producteur | 1 059 840 000 | 1 059 840 000 |

**HTTP transporte les 46 minutes ; le WAV n'en décrit que 15 min 32 s.**
La borne WAV laisse aussi un octet après la dernière trame complète de six
octets. Le transport seul ne prouve donc pas qu'un lecteur jouera tout.

Le lecteur indépendant est le module `wave` de Python, pas une seconde
implémentation de notre formule. Il reçoit l'en-tête extrait de la réponse
réelle et un fichier creux de la taille complète, avec le marqueur final
écrit à sa vraie position. Il se positionne sur sa dernière trame déclarée
et demande deux trames : il reçoit sept octets, puis EOF. Le marqueur existe
toujours au bout du fichier, mais ce lecteur ne peut pas l'atteindre par son
interface de lecture WAV.

Le fichier creux sert seulement à éprouver le lecteur sans écrire 6,36 Go
sur disque. Les **deux transferts HTTP précédents parcourent réellement
tous ces octets** ; ils ne sautent pas au dernier bloc.

Contrôle positif du lecteur : un WAV court dont la taille déclarée correspond
au contenu permet de lire le marqueur de la dernière trame, puis EOF.

## Ce que le code explique

- `audio/wav.rs::build_wav_header_with_duration` borne même une durée connue
  à `UNKNOWN_DATA_SIZE = i32::MAX - 36`.
- `audio/wav.rs::build_wav_header` utilise cette même borne sans durée.
- `StreamInfo::wav_content_length` calcule la longueur complète en `u64`.
- `handle_stream` annonce cette longueur et transmet tout le canal.

Le plafond signé a une raison de compatibilité écrite dans #1689. Le retirer
globalement ferait changer le contrat d'autres lecteurs ; le remplacer par
un autre entier de 32 bits ne décrit toujours pas 6 359 040 000 octets.
Les radios bornées, les reprises `Range`, les producteurs qui fournissent
leur propre en-tête et les porteurs DoP doivent être considérés dans une
correction du conteneur. Ce banc ne modifie aucun de ces comportements.

## Rejouer

Depuis un worktree isolé sur Shrek, avec le budget de compilation partagé :

```sh
export TUNE_TARGET_KEY=<cle-propre>
. /srv/cache/tune/env.sh
export CARGO_BUILD_JOBS=6
export TUNE_MEASURE_DIR="$(mktemp -d)"
cargo run --locked -j6 -p tune-stream-http --example mesure_long_wav_4016
python3 tune-stream-http/examples/inspect_long_wav_4016.py "$TUNE_MEASURE_DIR"
```

La commande Rust contrôle le transport et rend zéro si la longueur et le
marqueur final sont reçus. La commande Python imprime les mesures du lecteur
et rend **2** si le conteneur décrit moins de PCM que le corps HTTP. Sur cette
base, ce second verdict est rouge : le reproducer ne présente pas le défaut
restant comme un test vert. Les deux en-têtes conservés font 44 octets chacun.

Le programme est un exemple exécuté explicitement, pas un test automatique
de CI : une exécution transfère plus de 12 Go sur loopback. Une garde de
régression ordinaire devra accompagner le futur correctif.

## Contre-épreuve et limites

Une mutation temporaire du site de production qui calcule `wav_length` dans
`handle_stream` ramène le `Content-Length` à `i32::MAX`. Le banc refuse
cette réponse (sortie 101, gauche 2 147 483 647, droite 6 359 040 044) par l'assertion « HTTP doit annoncer toute la piste ».
Le code de production est ensuite restauré par copie ; les fichiers du banc
restent identiques. Les résultats de cette contre-épreuve et de la relance
sont archivés avec les mesures.

Il s'agit d'une **preuve de transport et d'une reproduction de troncature
par un lecteur logiciel**, pas d'une lecture sonore de 46 minutes.
Le banc n'exécute pas le décodage du FLAC original, les traitements DSP, une
sortie audio locale ou le renderer de Cyrille. Le PCM synthétique est compté
et son dernier marqueur vérifié, sans comparaison cryptographique de tous
les échantillons. On ne peut pas conclure que le testeur rencontre
effectivement une coupure à 15 min 32 s, ni que tous les lecteurs respectent
cette borne : certains peuvent lire jusqu'à la fin HTTP.

Aucune correction de production ni fermeture de #4016 n'est revendiquée.
Le défaut de conteneur est désormais reproductible. Une correction doit
permettre la lecture complète avec un conteneur accepté par la cible et
préserver les contrats de compatibilité existants. Si le conteneur change,
le lecteur indépendant du banc devra aussi savoir lire ce format : le module
Python utilisé ici éprouve le WAV/RIFF actuel, pas tous les conteneurs possibles.

Preuves : `/srv/builds/jp-evidence/jp-robbe-20260917-4016-long-stream`.

Dernière relance du transport : 59 462 ms puis 59 037 ms. La contre-épreuve
échoue sur l'assertion nommée ; restauration par copie puis deux transports
complets réussis. Le contrôle du conteneur reste rouge (sortie 2).
