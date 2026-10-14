# Chantier « lecture groupée » (#6132)

Go de Bertrand le 13/10/2026, après l'étude `docs/etude-6132.md` (PR #6302,
branche `chantier/entree-usb`). Branches du chantier : `chantier/lecture-groupee`
sur le serveur (depuis `batch/bugs-rc5-20261012`) et sur le client web (depuis
`main`).

## Décisions de Bertrand (13/10)

1. Une **vraie lecture groupée** : la même écoute part sur plusieurs zones.
   Aujourd'hui le groupement enregistre une liste et règle le volume ensemble,
   rien de plus.
2. Pour **deux sorties locales** (deux cartes son de la même machine), une
   **sortie double** qui écrit le même flux sur les deux périphériques, avec
   correction de dérive d'horloge (rééchantillonnage fin piloté par l'écart
   mesuré), pour rester synchrones.
3. **ASIO** garde « une zone à la fois » (limite du pilote) ; l'écran
   l'explique.
4. Les écrans Audio / Zones / Appareils deviennent deux écrans : **« Zones »**
   (ce que joue chaque zone) et **« Cette machine »** (les sorties de la
   machine). L'onglet Appareils disparaît. La Lecture groupée devient visible
   ailleurs qu'au clic droit.

## Étape 1 — la sortie double synchrone (serveur) : livrée (#6341)

### Le principe

Un pont d'horloges, comme `zita-ajbridge` ou `alsa_out` :

- le périphérique de la zone (**A**) est le maître : son rappel de rendu donne
  la cadence ;
- ce que A joue est recopié, période par période, vers le second périphérique
  (**B**), par un **rééchantillonneur sinc polyphasé** (64 coefficients,
  fenêtre de Blackman-Harris) dont le rapport vaut `cadence B / cadence A`
  corrigé de quelques ppm ;
- la correction est **pilotée par l'écart mesuré** : le remplissage de
  l'anneau de B, lissé, comparé à sa consigne. Un régulateur
  proportionnel-intégral (amorti critique) le ramène à la consigne ; sa part
  intégrale est l'estimation de la dérive des deux quartz ;
- A est retardé de la consigne (40 ms) par une ligne à retard, pour jouer au
  même instant que B. La consigne retranche le retard propre à B (la
  demi-largeur du filtre) ; l'étape 2a y ajoute la compensation de latence
  matérielle et mesure l'avance de B indépendamment de la phase des rappels ;
- deux régimes : un accrochage rapide pendant les 15 premières secondes (l'écart
  de départ vaut jusqu'à une période de A), puis une boucle lente qui ne suit
  plus que la dérive, sans moduler la hauteur.

### Où c'est dans le code

| Fichier | Rôle |
|---|---|
| `tune-core/src/outputs/sortie_double.rs` | le recopieur (filtre, régulateur, ligne à retard), sans cpal ni `local-audio` : jugé sous Linux |
| `tune-core/src/outputs/sortie_double_tests.rs` | le banc : deux périphériques simulés, d'horloges différentes, plusieurs minutes |
| `tune-core/src/outputs/local/sortie_double_locale.rs` | ouverture de B sur le même hôte cpal, son rappel, l'emplacement que le rappel de A consulte, les refus |
| `tune-core/src/outputs/local/backend.rs` | le rappel `f32` partagé de A appelle `recopier_la_periode` ; `BackendCpal::brancher_la_seconde_sortie` |
| `tune-core/src/outputs/local.rs` | `LocalOutput::set_seconde_sortie` ; `play_url` branche B après l'ouverture de A, avant son démarrage |
| `tune-core/src/orchestrator/transport.rs` | à chaque lecture locale, pose la seconde sortie lue dans le réglage `zone_{id}_sortie_double` |

Le chemin d'appel réel : `transport.rs` (lecture d'une zone locale) →
`LocalOutput::set_seconde_sortie` → `play_url` → `BackendCpal::ouvrir` →
`brancher_la_seconde_sortie` → `ouvrir_la_seconde_sortie` (B ouvert, recopie
installée) → à chaque période, le rappel de A → `recopier_la_periode`.

Le réglage `zone_{id}_sortie_double` (valeur `local:<nom>` ou `<nom>`) reste
**interne** (décision du 13/10) : seule la route de groupement l'écrit
(étape 2a).

### Ce qui est refusé, et dit dans le journal (`sortie_double_refusee`)

Dans ces cas la zone joue sur A seule, jamais de silence :

- **ASIO** (`asio_un_seul_flux`) : un seul flux par processus ;
- **mode exclusif** WASAPI ou CoreAudio (`mode_exclusif`), **chemin compressé**
  (`chemin_compresse`), **format entier** de A (`format_de_a`) : la recopie
  n'existe que sur le rappel `f32` partagé ;
- **« bit-perfect strict »** (`bitperfect_strict`) : B est rééchantillonné ;
- B introuvable sur l'hôte ouvert (aucun repli sur la sortie par défaut), B
  identique à A, ou B sans le nombre de canaux de A.

À l'arrêt, `sortie_double_releve` journalise l'écart, la dérive estimée, la
correction et les échantillons refusés par l'anneau de B.

### Mesure (banc simulé, Shrek)

Passage du 13/10 (`cargo test -p tune-core --lib sortie_double`) :

- 48 kHz / 48 kHz, A à +100 ppm, B à −80 ppm (10,8 ms de dérive par minute sans
  correction), périodes de 480 et 512 trames, **10 minutes** : écart A/B de
  0,19 ms au pire entre 20 s et 10 min (0,03 ms après la première minute),
  0,008 ms à la fin, dérive estimée −179,4 ppm pour −180, aucune famine de B ;
- 44,1 kHz vers 48 kHz, ±250 ppm, stéréo, 4 minutes : écart de 0,05 ms au pire
  entre 20 s et 4 min, dérive estimée +500,4 ppm pour +500 ;
- le signal recopié est à la position annoncée à 5,5·10⁻⁷ près (sinus 1 kHz,
  +1 000 ppm, correction en mouvement) ;
- au départ, l'écart vaut jusqu'à une période de A (B ne sait pas où A en est
  dans sa période) et se résorbe en moins de 5 s.

Ce que le banc ne mesure pas : la **latence matérielle** de chaque carte (tampon
du pilote, convertisseur). Elle est constante, elle ne dérive pas, mais elle
diffère d'une carte à l'autre (quelques ms). Voir les décisions.

## Décisions de Bertrand sur l'étape 2 (13/10, après l'étape 1)

1. La latence propre à chaque carte est compensée **automatiquement** par les
   horodatages cpal ; `sync_delay_ms` reste en réglage de secours.
2. Les 40 ms de retard ajoutés aux deux sorties sont acceptés.
3. **Volume séparé par membre** du groupe, plus un volume général qui les règle
   ensemble.
4. `zone_{id}_sortie_double` reste interne, posé seulement quand on groupe deux
   zones, par la route de groupement.
5. Refus en « bit-perfect strict », acceptation en mode PURE : maintenus.
6. Mode exclusif et flux compressé : **après** l'étape 2 (étape 2c ci-dessous).

## Étape 2a — groupe de deux zones locales (serveur) : livrée (#6342)

- **Route de groupement** (`POST /zones/group`, `PATCH /zones/groups/{id}`,
  `DELETE`) : quand le meneur est une zone locale et qu'un autre membre est une
  zone locale de la machine, la route pose `zone_{meneur}_sortie_double` (la
  sortie du suiveur) et l'écrit dans le groupe (`sortie_double`) ; dégrouper ou
  retirer le membre l'enlève. `choisir_la_sortie_double` : un seul suiveur
  local (la recopie va vers UN périphérique).
- **ASIO** : deux zones locales dont l'une sort par ASIO sont refusées
  (`409 group_asio_one_zone`, phrase « une zone à la fois sur cette machine »
  dans les dix langues) — la phrase que l'écran affichera.
- **Contrat de synchronisation** : un groupe à sortie double publie
  `supported: true`, `transport: "local_double_output"`,
  `render_latency_calibrated: true`, les zones synchronisées et les zones
  indépendantes. `accuracy_claim_ms` reste `null` tant qu'aucune mesure sur
  vraies cartes n'existe.
- **Latence matérielle** : chaque rappel lit `playback − callback` dans ses
  horodatages cpal ; B est nourri d'autant plus tôt que sa latence dépasse
  celle de A. `sync_delay_ms` des deux zones (B − A) s'y ajoute, et la remplace
  quand le pilote ne donne pas d'horodatage. Borne : avec la consigne de 40 ms
  et des périodes de 10 ms, B peut être avancé d'environ 25 ms sur A.
- **Mesure indépendante de la phase** : l'avance de B est reconstituée en
  continu (`en attente + période lue − écoulé depuis la lecture × cadence`).
  Mesurée à l'instant où A passe, elle dépendait de la phase entre les deux
  rappels ; deux cartes aux périodes égales (10 ms en WASAPI partagé) gardent
  la même phase des minutes durant, et l'écart se calait jusqu'à une
  demi-période à côté.
- **Volume par membre** : quand B joue, A rend sa période au volume unité, la
  recopie, puis applique son propre volume ; B applique le curseur de SA zone
  (celui de `set_volume` et de la sourdine de la zone B), composé avec le
  ReplayGain et la compensation de la piste de A, avec une rampe sur la
  période. Le volume général du groupe (`PUT /zones/groups/{id}/volume`,
  maître + écarts par membre) agit donc sur les deux sorties.
- **DoP** : B se tait (un DoP rééchantillonné n'est plus du DSD).

## Décisions de Bertrand du 14/10 (après #6342)

1. Plusieurs zones locales suiveuses par groupe : trois cartes ou plus,
   chacune corrigée de sa dérive.
2. Une carte plus lente de plus de 25 ms : le retard commun s'adapte
   automatiquement à la plus lente.
3. Meneur réseau qui synchronise aussi les zones locales : étape 2b.
4. Les groupes enregistrés avant cette version sont recalculés au démarrage.

## Étape 2a bis — trois cartes ou plus, retard commun adaptatif (serveur)

- **Plusieurs suiveuses** : `choisir_les_sorties_doubles` retient chaque
  autre membre local de la machine (une recopie par périphérique distinct).
  `zone_{meneur}_sortie_double` devient un tableau JSON (la forme à une
  sortie de l'étape 2a reste lue). Une [`SortieMultiple`] sert toutes les
  suiveuses depuis le rappel de A : un recopieur par suiveuse (sa dérive, sa
  latence, son volume), UNE ligne à retard pour A.
- **Retard commun adaptatif** : il vaut la consigne (40 ms) tant qu'elle
  suffit. Si une carte a trop de latence pour être nourrie à temps (au-delà
  d'environ 25 ms de plus que A), il grandit jusqu'à ce qu'elle tienne sa
  période entière, borné à 40 + 200 ms. Il est réglé pendant le silence de
  départ — les latences sont lues dès les premiers rappels, chaque B publie
  sa période avant même de lire — puis figé quand le premier échantillon réel
  sort de A, parce qu'un changement en cours d'écoute s'entendrait. Le
  rappel de B ne part jamais sous une période plus 5 ms, même avant que le
  retard commun soit réglé.
- **Recalcul au démarrage** : `recalculer_les_sorties_doubles` repasse chaque
  groupe enregistré et pose ses suiveuses (ASIO : retirées, le groupe reste
  un groupe de volume). Sortie pas encore enregistrée au démarrage : le
  backend local réglé pour la machine décide.

## Réajustement du retard commun (décision du 14/10)

Le retard commun se réajuste **à la pause et à la piste suivante, jamais
pendant le son** :

- une pause (rappel de A en pause) et un enchaînement de piste
  (`local_audio_gapless_chaining_next_track`) DEMANDENT le réajustement ; une
  piste rouverte le recalcule d'office à l'ouverture ;
- la demande n'est servie que lorsque A ne reçoit plus que du silence depuis
  au moins la longueur de la ligne à retard (l'ancienne comme la nouvelle) :
  tout ce que A et les B ont encore à jouer est alors silencieux. Un
  enchaînement sans blanc attend donc le prochain silence ;
- au réajustement, chaque B est recalé d'un coup sur sa nouvelle consigne
  (zéros ajoutés ou trames sautées, dans le silence), sa dérive estimée
  conservée.

ASIO (décision du 14/10) : le choix de #6358 reste — quand la sortie n'est pas
encore connue, le backend réglé pour la machine tranche.

## Étape 2b — lecture groupée avec des membres réseau (serveur)

1. **Propager la lecture d'un groupe.** À la lecture sur le meneur, les suiveurs
   réseau suivent : même élément, même position, mêmes commandes (pause,
   reprise, saut, suivante). Le groupe devient un état du transport, pas
   seulement une liste dans `zone_groups`. La zone suiveuse locale d'une
   sortie double doit aussi s'afficher « en groupe avec » le meneur.
   **Meneur réseau** (décision du 14/10) : ses membres locaux de la machine
   jouent aussi ensemble, synchronisés entre eux ; aujourd'hui seul un
   meneur local porte la sortie double.
2. **Transport par membre** : points de diffusion OAAT → groupe OAAT existant ;
   renderers indépendants (UPnP, AirPlay, Chromecast…) → départs alignés au
   mieux, sans garantie d'horloge, dit dans l'interface.
3. **Brancher ce qui est enregistré sans consommateur** : `calibrate_group`
   (501), `GroupManager` (sans appelant), `sync_delay_ms` pour les membres
   réseau.
4. **Une seule famille de routes** : `/zone-manager/groups` écrit le même
   réglage que `/zones/group` sans validation ni contrôle Premium ; la ramener
   sur la validation commune ou la retirer.
5. `docs/getting-started` : le chemin « Groupes de zones » n'existe plus.

## Étape 2c — la sortie double hors du chemin partagé (serveur)

Décision du 13/10 : après l'étape 2.

- **mode exclusif** (WASAPI exclusif, CoreAudio « hog ») : la recopie depuis le
  rappel natif, B en exclusif ou partagé ;
- **flux compressé** (non-WAV, décodé dans la sortie) : le même branchement
  sur `build_compressed_f32_stream` ;
- adaptation des canaux, relevé dans l'API de diagnostic ;


## Étape 3 — les écrans (client web)

1. **« Zones »** : ce que joue chaque zone. Une carte par zone qui ouvre tous ses
   réglages (sortie, audio, canaux, correction de pièce) ; la fenêtre du clic
   droit devient un raccourci vers cette carte. Les deux interrupteurs de
   Réglages ▸ Zones y viennent.
2. **« Cette machine »** : les sorties de la machine — backend et mode exclusif,
   visibilité des sorties et des appareils, services réseau.
3. L'onglet **Appareils** disparaît (la sauvegarde des réglages trouve une
   autre place).
4. **Lecture groupée visible** : un geste « Grouper » sur la page Zones, les
   groupes affichés comme tels (membres, transport utilisé, synchronisé ou
   non), dégrouper ; la pastille de la barre de lecture montre le groupe.
5. **ASIO** : l'écran explique « une zone à la fois sur cette machine » là où
   le groupement est refusé.

## Reste ouvert

- l'essai sur deux vraies cartes (Windows WASAPI partagé, Linux ALSA), qui
  donnera `accuracy_claim_ms` ;
- les groupes enregistrés avant l'étape 2a n'ont pas de sortie double tant
  qu'ils ne sont pas refaits (ou modifiés).

Refs #6132
