# Banc matériel du #2218 — protocole des essais qu'aucune machine de compilation ne peut faire

**État : protocole, pas résultat.** Ce fichier dit *quel matériel*, *combien de
temps*, *avec quel stimulus*, *ce qu'on relève*, *ce qui constitue un échec* et
*où le résultat se consigne*. Il ne contient **aucun chiffre de terrain** :
aucun n'a été relevé à ce jour, et aucun ne doit être ajouté ici sans sa fiche
d'essai.

## La règle

> **Aucune revendication chiffrée sans protocole, matériel et distribution publiés.**

C'est la case que #2218 porte depuis son ouverture sous la forme « Aucun chiffre
marketing « < 1 ms » sans protocole, matériel et distribution publiés », et que
personne n'avait encore énoncée ailleurs que dans une case à cocher. Elle vaut
pour la latence, la sous-alimentation, la gigue, la dérive d'horloge et la
synchronisation multiroom, dans la documentation comme dans l'interface.

Elle est tenue par un témoin : `tune-output-api/tests/revendication_chiffree_2218.rs`.
Ce qu'il couvre et ce qu'il ne couvre pas est écrit à la fin de ce document.

## Pourquoi ce document existe

Quatre cases de #2218 exigent du matériel, et aucune ne portait de protocole
écrit :

| case de #2218 | essai ci-dessous |
|---|---|
| Compteurs d'underrun/xrun et tests de charge 1 h / 8 h / 24 h | **E1**, **E2**, **E3** |
| Matrice Windows ASIO/WASAPI, macOS CoreAudio et Linux ALSA | **E4**, **E5** |
| Validation end-to-end par loopback ou microphone sur plusieurs DAC | **E6** |
| Mesures p50/p95/p99 des timestamps de présentation | **E6** — *l'instrument n'existe pas ; voir « Ce qui n'est pas instrumenté »* |

La machine de compilation du projet n'a **pas de carte son**, et WASAPI comme
CoreAudio n'y compilent pas. Rien de ce qui suit n'a donc été exécuté, et rien
de ce qui suit ne peut l'être par la CI. C'est la raison d'être d'un protocole
écrit : il rend l'essai reproductible par quelqu'un qui a le matériel, et il
rend visible l'écart entre « mesuré » et « supposé ».

## L'instrument

**On ne mesure qu'avec ce qui est déjà branché.** L'instrument de tous les
essais de charge est `RingStarvation` (`tune-output-api/src/lib.rs`), relevé par
`RingStarvation::snapshot`. Il est compté dans le **drain de l'anneau**, l'objet
que les chemins de rendu traversent tous, et il est consommé aujourd'hui par le
sondeur (`tune-core/src/poller.rs`, qui appelle `ring_starvation()` à chaque
tick) et par le banc `tune-core/src/poller/rappel_arrete_3814.rs`. Rien de neuf
n'est à écrire pour mesurer : il suffit de lire.

`snapshot` rend cinq champs, et les cinq sont nécessaires :

| champ | ce qu'il dit |
|---|---|
| `events` | rappels du pilote servis à court — des **zéros sont partis vers le DAC** |
| `missing_samples` | combien d'échantillons ont manqué, cumulés : un micro-trou et une coupure d'une seconde ne se ressemblent pas |
| `driver_underruns` | le **pilote** n'a pas été servi à temps (XRun). Panne disjointe de la précédente, **jamais additionnée** : sur un XRun, cpal saute le rappel de données et l'anneau reste plein |
| `served_samples` | le dénominateur de `missing_samples` |
| `stream_ms` | la durée d'audio écoulée, **déduite du compte d'échantillons**, jamais d'une horloge |

Le chemin du signal se lit à côté, sans instrumentation nouvelle :
`OutputSignalPathStatus` (`bit_perfect`, `sample_transport`, `dsp`, `volume`,
`reasons`), avec `OutputSampleTransport` (`NativeInteger` / `Float`) et
`OutputDspState` (`Inactive`, `Applied`, `BypassedPure`, `BypassedDop`,
`Unknown`).

### Où ces chiffres se lisent

| source | portée | remise à zéro |
|---|---|---|
| `GET /api/v1/system/diagnostics` → `ring_starvation[]` | par sortie | **à chaque piste** |
| `GET /api/v1/devices/{id}/buffer-stats` et `/api/v1/devices/buffer-stats/all` | par sortie | **à chaque piste** |
| `GET /api/v1/zones/{id}/signal-path` | par zone | à chaque piste |
| rapport de bogue (`/api/v1/system/bug-report`) | par sortie | **à chaque piste** |
| journal : `famine_anneau_debut` / `famine_anneau_fin` | par zone, **daté**, cumulatif par épisode | jamais |

Les noms de champs des routes : `total_underruns` (= `events`, nom historique),
`ring_starvation_missing_samples`, `driver_underruns`, `served_samples`,
`stream_ms`. `null` veut dire « cette sortie n'observe pas sa famine » — ce
n'est **pas** un zéro.

⚠️ **Les compteurs des routes repartent de zéro à chaque piste.** Un essai de 1 h
ne laisse donc dans `diagnostics` que le relevé de la dernière piste. **Le seul
relevé cumulatif sur une fenêtre longue est le JOURNAL**, deux lignes par
épisode :

```text
WARN famine_anneau_debut zone_id=… device=… flux_ms=… rappels_a_court=… silence_ms=…
WARN famine_anneau_fin   zone_id=… device=… flux_ms=… rappels_a_court=… \
                         echantillons_manquants=… silence_ms=… duree_ms=…
```

Les trois pièges déjà établis par `docs/mesures/3205-noyau-rt-tune-os.md`
s'appliquent tels quels et ne sont pas répétés ici : une zone réseau ne mesure
rien, un zéro sans dénominateur n'est pas un zéro, la machine de mesure doit
être au repos.

## Ce qui n'est PAS instrumenté

Ces manques décident du sort de plusieurs essais ci-dessous. Les nommer fait
partie du protocole : un essai qui rendrait un chiffre sans instrument rendrait
une invention.

* **Les timestamps de présentation ne sont pas mesurés.**
  `POST /api/v1/zone-manager/measure-latency` publie un aller-retour de
  **commande** (`control_rtt` : `min_ms`, `p50_ms`, `p95_ms`, `p99_ms`,
  `max_ms`, `uncertainty_ms`) et pose explicitement `audio_latency_ms: null`.
  `GET /api/v1/zone-manager/sync-status` rend `max_drift_ms` avec
  `measurement: "reported_playback_position"` et
  `synchronization_guarantee: false`. **Aucun demi-RTT ne doit être republié
  comme latence de restitution** (#2215). La case « p50/p95/p99 des timestamps
  de présentation » de #2218 ne peut donc pas être cochée par une mesure : elle
  attend d'abord un instrument.
* **Aucun compteur de sous-alimentation sous la couche pilote.** Ce que Tune
  compte est *au-dessus* du pilote. `/proc/asound` n'a pas été vérifié sur une
  machine réelle et ce protocole ne s'appuie pas dessus.
* **Aucun compteur de déconnexion** n'existe dans l'arbre (`total_disconnections`
  vaut `null`, et c'est délibéré).

## La fiche d'essai

Aucun relevé n'est recevable sans cette fiche **complète**. Un champ inconnu
s'écrit « inconnu », jamais une valeur plausible.

```yaml
essai:              E1|E2|E3|E4|E5|E6
date:               AAAA-MM-JJ
opérateur:
matériel:
  interface:        marque, modèle, révision       # ex. Topping E30, USB
  liaison:          USB | I²S | S/PDIF | AES | réseau
  pilote:           nom + version exacte           # pilote du fabricant ou générique, version complète
  os:               nom + build exact              # le numéro de build, pas « Windows 11 » ni « macOS » seuls
  noyau:            standard | PREEMPT_RT          # Linux/Tune OS uniquement
  machine:          CPU, RAM, alimentation secteur ou batterie
serveur:
  version:          v0.9.x
  commit:           sha court
  audio_backend:    la valeur de `audio_backend` DANS /system/diagnostics
  audio_backend_status: la valeur de `audio_backend_status`
  asio_available:   true|false
  reglage_tampon:   la valeur de `buffer_s`, et `auto`
zone:
  type:             sortie locale | renderer réseau (OAAT) | autre
  dsp:              égaliseur, convolveur, crossfeed, volume — état exact
stimulus:
  contenu:          fichier ou service, format, cadence, profondeur
  duree_visee:      1 h | 8 h | 24 h
charge_de_fond:     aucune | scan | ReplayGain | … (à proscrire, voir plus bas)
```

🔴 **`audio_backend` se lit dans le diagnostic, jamais dans le nom de la zone.**
Une zone nommée « USB DAC ASIO » peut tourner en WASAPI ; le nom est une
étiquette, `audio_backend` est le moteur.

## Les essais

### E1 — Charge 1 h, sortie locale

* **Matériel** : une machine, une interface, une seule zone en **sortie
  locale**. Noyau et pilote notés dans la fiche. Machine **au repos** : ni scan
  de bibliothèque, ni passe ReplayGain, ni compilation en fond.
* **Durée** : 60 min de lecture **continue**, sans interaction.
* **Stimulus** : un contenu représentatif de l'usage visé, joué en boucle sans
  changement de format. Si la question porte sur le hi-res, mesurer en hi-res.
  Le format est noté dans la fiche : un essai 16/44,1 et un essai 24/192 ne se
  comparent pas.
* **Relevé** :
  1. le journal sur la fenêtre — `grep -c famine_anneau_fin` donne le **nombre
     d'épisodes** ; la somme des `silence_ms` donne le **silence total envoyé au
     DAC** ; la somme des `duree_ms` donne la largeur d'audio couverte ;
  2. `GET /api/v1/devices/buffer-stats/all` **à la fin de la dernière piste,
     avant qu'elle ne change**, pour `driver_underruns`, `served_samples` et
     `stream_ms` ;
  3. `GET /api/v1/zones/{id}/signal-path` au début et à la fin : `bit_perfect`,
     `sample_transport`, `dsp`, `volume`, `reasons`.
* **Échec** : au moins un épisode de famine (`events > 0`), **ou** au moins une
  sous-alimentation du pilote (`driver_underruns > 0`), **ou** un changement non
  demandé de `bit_perfect` / `dsp` / `sample_transport` pendant la fenêtre.
  Le seuil est zéro et il ne se négocie pas : chaque événement est un trou
  **audible**, comblé par des zéros partis vers le DAC.
* **Essai invalide** (à refaire, pas à interpréter) : `served_samples` ou
  `stream_ms` nul ou `null` ; somme des `duree_ms` franchement inférieure à la
  fenêtre — la lecture s'est arrêtée ; charge de fond non nulle.
* **Consignation** : fiche + les deux nombres (épisodes, `silence_ms` total) +
  l'extrait de journal brut + la réponse JSON de `buffer-stats/all`.

### E2 — Charge 8 h

* **Matériel** : identique à **E1**, et il ne change pas d'un essai à l'autre :
  E1, E2 et E3 ne se comparent que sur la même fiche matérielle.
* **Durée** : 8 h continues.
* **Stimulus** : identique à **E1**, même contenu et même format.
* **Relevé** : celui de **E1**, plus **la date de chaque épisode**. Une fenêtre
  de 8 h sert à voir si les épisodes se groupent (une tâche périodique, une mise
  en veille de disque, une rotation de journal) ou se répartissent.
* **Échec** : le même zéro qu'en E1. Un épisode unique en 8 h reste un échec —
  il est simplement plus difficile à trouver.
* **Consignation** : celle de E1, avec la liste horodatée des épisodes.

### E3 — Charge 24 h

* **Matériel** : identique à **E2**.
* **Durée** : 24 h continues.
* **Stimulus** : identique à **E2**.
* **Relevé** : celui de **E2**, plus `memory_rss_mb` et `uptime_seconds` de
  `/api/v1/system/diagnostics` au début, à mi-parcours et à la fin.
  `process_started_at` **doit être identique aux trois relevés** : s'il change,
  le serveur a redémarré et l'essai est nul.
* **Échec** : le zéro de E1, **plus** tout redémarrage du processus pendant la
  fenêtre.
* **Consignation** : celle de E2, plus les trois relevés de diagnostic complets.

### E4 — Matrice des moteurs audio

Un même contenu, une même machine par ligne, **une ligne par moteur réellement
actif**. La matrice n'a de sens que si chaque ligne porte son `audio_backend`
relevé dans le diagnostic.

| ligne | OS | moteur attendu | à noter en plus |
|---|---|---|---|
| M1 | Windows | ASIO (mode exclusif) | `asio_available`, nom et version du pilote ASIO du fabricant |
| M2 | Windows | WASAPI exclusif | l'identifiant d'endpoint réellement ouvert, pas le nom d'affichage |
| M3 | macOS | CoreAudio | périphérique agrégé ou non, taux forcé ou non dans « Configuration audio et MIDI » |
| M4 | Linux | ALSA | **le nom PCM réellement ouvert** : `hw:CARD=…` atteint le pilote, `default`, `sysdefault:`, `dmix:` et `plughw:` passent par un greffon qui accepte toutes les cadences et convertit en silence |

* **Matériel** : une fiche par ligne. Les quatre lignes vivent sur trois OS
  différents : ce sont donc trois machines au moins, et la matrice ne compare
  **pas** les machines entre elles — elle constate, ligne par ligne, qu'un
  moteur tient ou ne tient pas une heure.
* **Durée** : 1 h par ligne (le protocole de **E1**, appliqué quatre fois).
* **Stimulus** : **le même contenu sur les quatre lignes**, sans quoi la
  comparaison ne dit rien. Répéter la matrice entière par format
  (16/44,1 puis 24/96 puis 24/192) plutôt que mélanger les formats entre lignes.
* **Relevé** : le relevé de **E1**, plus `bit_perfect` et `sample_transport`
  pour chaque ligne. Une ligne dont `sample_transport` vaut `Float` alors que le
  contenu est entier se consigne telle quelle : c'est un résultat, pas une
  erreur de manipulation.
* **Échec** : celui de **E1** pour chaque ligne, **et** toute ligne dont
  `audio_backend` diffère du moteur attendu — cette ligne n'a pas mesuré ce
  qu'elle prétend mesurer, et son chiffre ne doit pas entrer dans la matrice.
* **Consignation** : une fiche par ligne. Une matrice partielle se publie
  partielle, avec les lignes manquantes nommées « non mesuré ».

### E5 — WASAPI : le bon endpoint, et pas de repli silencieux

Ce protocole n'est pas nouveau : il a été écrit par Jean-Philippe Robbe dans
#2207 et n'avait jamais été exécuté. Il est repris ici mot pour mot dans son
intention, parce qu'un protocole qui vit dans un commentaire d'issue fermée
n'est pas publié.

* **Matériel** : un poste **Windows** avec **deux** endpoints de rendu
  distincts, dont un DAC qui n'est **pas** le périphérique par défaut.
* **Durée** : ponctuel, pas de charge. Quatre gestes.
* **Stimulus** : n'importe quel contenu qui joue sans interruption pendant les
  quatre gestes ; ce qui est mesuré ici est la **résolution du périphérique**,
  pas le signal. Les gestes :
  1. configurer la zone sur le DAC **par son identifiant stable**, pas par son
     nom d'affichage, et lancer la lecture ;
  2. vérifier dans le journal **l'identifiant et le nom réellement ouverts** ;
  3. **changer le périphérique Windows par défaut pendant la lecture** et
     vérifier que la zone reste sur le DAC ;
  4. **débrancher le DAC** et vérifier un **échec explicite**, sans son sur le
     nouvel endpoint par défaut.
* **Relevé** : les lignes de journal d'ouverture des gestes 1 et 2, la valeur de
  `audio_backend` et de `audio_backend_status` dans le diagnostic, et le message
  d'erreur exact du geste 4.
* **Échec** : le son continue sur un autre endpoint au geste 3 ou 4 ; ou le
  journal ne nomme pas l'endpoint ouvert ; ou l'échec du geste 4 est silencieux.
* **Consignation** : fiche + les lignes de journal brutes + le message d'erreur
  **recopié**, jamais résumé.

### E6 — Boucle multiroom

* **Matériel** : au moins **deux** points de diffusion Tune **regroupés par
  OAAT** — seuls ceux-là partagent une horloge et reçoivent des timestamps de
  présentation. Un groupe DLNA, AirPlay ou Chromecast ne mesure rien ici : Tune
  ne le présente pas comme un système synchronisé, et le délai par zone y est
  une correction manuelle. Plus : une **capture** — boucle électrique (loopback)
  entre les deux sorties dans une même interface d'entrée, ou deux micros de
  mesure — et l'entrée d'une interface capable d'enregistrer les deux canaux sur
  **une seule horloge d'échantillonnage**.
* **Durée** : 15 min de lecture groupée, puis trois cycles
  arrêt / regroupement / relance.
* **Stimulus** : un contenu portant un **transitoire net et répété** (clic,
  claquement) — la mesure est faite sur la capture, par corrélation croisée
  entre les deux canaux, hors de Tune.
* **Relevé** :
  1. la **capture**, en fichier, conservée avec la fiche. C'est elle, et elle
     seule, qui porte l'écart de restitution ;
  2. l'écart mesuré sur la capture, sa méthode (fenêtre, corrélation, cadence de
     l'enregistrement) et son **incertitude** ;
  3. côté Tune, `RingStarvation::snapshot` pour chaque sortie du groupe — un
     écart accompagné de famine dit d'abord que le producteur a décroché, pas
     que l'horloge a dérivé ;
  4. `POST /api/v1/zone-manager/measure-latency`, **consigné comme RTT de
     commande** et rien d'autre.
* **Échec** : famine non nulle sur l'une des sorties ; ou un écart qui **dérive**
  d'un cycle à l'autre, ce qui accuse l'horloge et non un décalage constant.
* 🔴 **Ce que cet essai ne peut pas rendre aujourd'hui** : les **p50/p95/p99 des
  timestamps de présentation**. Rien dans l'arbre ne les enregistre. Une
  distribution obtenue par capture décrit le **résultat acoustique** de la
  chaîne entière, ce qui est utile et suffisant pour E6, mais ce n'est pas la
  même grandeur et les deux ne doivent pas se publier sous le même nom. Tant que
  l'instrument n'existe pas, la case correspondante de #2218 reste décochée et
  **aucun chiffre de présentation ne se publie**.
* **Consignation** : fiche + le fichier de capture + la méthode d'analyse
  **versionnée** (sur le modèle de `tune-aes17-oriented-residual-v1`, voir
  `docs/audio-conformance-aes17.md`) + les relevés Tune.

## Comment le résultat se consigne

1. **Un fichier par campagne**, nommé `docs/mesures/2218-resultats-AAAA-MM-JJ.md`,
   contenant les fiches complètes et les relevés bruts. Les relevés bruts sont
   recopiés, jamais résumés : une somme se recalcule, un extrait perdu ne se
   retrouve pas.
2. **Un commentaire sur #2218** renvoyant à ce fichier, et cochant **uniquement**
   les cases que la campagne couvre réellement.
3. **La distribution publiée avec le chiffre.** Un nombre seul n'est pas un
   résultat : il va avec son dénominateur (`served_samples`, `stream_ms`), sa
   fenêtre, son matériel et son incertitude quand il en a une.
4. **Une case non mesurée reste décochée**, et le fichier de campagne la nomme
   « non mesuré » plutôt que de l'omettre. Une omission se relit comme un
   succès.

## Ce que ce document ne permet pas de dire

* Il ne permet **aucune** affirmation sur la latence de restitution : rien dans
  l'arbre ne la mesure.
* Il ne permet pas de conclure sur le noyau `PREEMPT_RT` de Tune OS — c'est un
  autre protocole, `docs/mesures/3205-noyau-rt-tune-os.md`, qui compare **une
  seule machine sur deux noyaux**. E1 à E3 ne comparent rien : ils constatent.
* Il ne permet pas de comparer deux machines entre elles. Toute comparaison
  publiée doit avoir changé **une seule** variable.

## La garde

`tune-output-api/tests/revendication_chiffree_2218.rs` tient deux choses, et
elle lit du **texte** : elle n'a besoin d'aucune caractéristique de compilation
et tourne donc sur toutes les PR.

1. Ce document existe, porte la phrase de la règle mot pour mot, nomme
   `RingStarvation::snapshot`, et chaque essai E1 à E6 porte ses rubriques
   (**Matériel**, **Durée**, **Stimulus**, **Relevé**, **Échec**,
   **Consignation**).
2. Dans un **périmètre de fichiers nommé un à un**, aucune revendication de la
   forme « moins de N millisecondes » portant sur la latence, la gigue, la
   famine, la sous-alimentation ou la synchronisation ne peut apparaître sans
   que la section qui la contient renvoie à ce document.

Ce que la garde **ne** couvre pas est écrit dans son en-tête, et doit y rester à
jour : c'est la seule façon qu'a la tranche suivante de savoir où reprendre.
