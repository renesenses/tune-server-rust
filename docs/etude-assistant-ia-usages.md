# Étude #6834 — de nouveaux usages pour l'assistant IA, avec la clé de l'utilisateur

*Étude seule, aucun code de production. Base lue : `batch/bugs-rc8-20261022`
(`084d01d41`). Date : 23/10/2026. Décision de Bertrand du 23/10/2026.*

Rappel du cadre (`docs/chantier-assistant-ia.md`) : la fonction est
**gratuite**, la clé est celle de l'utilisateur, les appels partent de **son**
serveur vers **son** fournisseur (Anthropic, compatible OpenAI, Gemini ou
Ollama en local), et rien ne passe par mozaiklabs.fr. Chaque appel est compté
au tarif daté de `couts.rs` et borné par le plafond mensuel (2 € par défaut).

## 0. Résumé

- **Le socle est déjà là.** Le greffon `tune-assistant-ia` offre un appel
  compté et plafonné (`appel::appeler_avec`), un cache, une validation par
  schéma côté serveur, et le trio appliquer / annuler / historique. Le
  dialogue à outils (`/ai/query`) fonctionne avec les quatre familles de
  fournisseurs. Chacune des sept pistes est d'abord **une consigne, un
  contexte borné et un validateur** ajoutés à ce socle, pas un nouveau
  sous-système.
- **Ce qui coûte, ce sont les jetons d'entrée.** Aux prix de `couts.rs`
  (Haiku 5.5 : 0,10 $ / 0,50 $ par million de jetons ; Sonnet 5.5 : 2 $ /
  10 $), presque tous les usages coûtent **moins d'un centime avec Haiku**.
  Seul le diagnostic, parce qu'il lit des journaux, dépasse le centime avec
  Sonnet (§ 1).
- **Le vrai risque n'est pas le coût mais ce qui sort de la machine.** Le
  nettoyage actuel (`tune_core::confidentialite::anonymiser`) a été conçu
  pour un forum public : il **garde** les adresses IP du réseau local et les
  noms d'appareils, parce qu'ils servent au diagnostic. Avant d'envoyer quoi
  que ce soit à un fournisseur tiers, il faut une passe plus stricte (§ 1.4).
- **Le volume reste au serveur.** Les limites de #6541
  (`tune-core/src/audio/volume_limites.rs`) s'appliquent déjà à toute
  consigne, y compris à celles de l'outil `set_volume` de `/ai/query`. Mais
  cet outil accepte une valeur absolue, et **un argument absent y vaut 0,5**
  (`tune-core/src/ai/executor.rs`, `set_volume`). Il faut corriger cela avant
  d'ouvrir l'assistant à la voix (§ 6).
- **Recommandation.** Pour la rc8 : uniquement des usages en lecture seule et
  des correctifs du socle (le diagnostic en version « explique-moi », les
  notes de pochette, le correctif du volume). Après la v1 : les identités,
  le DJ qui explique, les commandes en langage courant, le bilan d'écoute.
  À ne pas faire : la vision envoyée par défaut, une hausse de volume ou de
  gain décidée par le modèle, un outil Home Assistant générique (§ 8).

## 1. Diagnostic de son propre serveur (« Pourquoi ma zone ne joue pas ? »)

### 1.1 Valeur

C'est la question la plus fréquente sur le forum, et la plus chère en temps
pour l'équipe. Quelques fils récents de ce type :
#6378 (une zone DLNA masquée disparaît de `GET /zones` à chaque démarrage, et
le journal le dit en toutes lettres : `ssdp_startup_zone_hidden_skipping`),
#6753 (OpenHome qui reprend sa file), #6333 (plus de son après une avance
dans la piste), #6751 (AirPlay 1 bloqué à 0:00), #4704 (« la musique ne se
lance pas »). Dans plusieurs de ces fils, la cause, ou au moins le bon
indice, était **déjà dans le journal ou dans `/system/diagnostics`** ; il
manquait quelqu'un pour le lire. Un modèle qui lit ces sources sur le
serveur de l'utilisateur et répond « voici ce que je vois, voici ce que vous
pouvez essayer, voici le rapport à envoyer s'il le faut » fait gagner un
aller-retour au testeur comme à l'équipe.

### 1.2 Existant et points d'accroche

| Source | Où | Remarque |
|---|---|---|
| Journaux récents | `collect_recent_logs(max_lines)` dans `tune-server/src/routes/system/diagnostics.rs` (l. 1608), servi par `GET /system/logs` | lit une fin de fichier bornée (`LOG_TAIL_BYTES`), répartit la fenêtre entre les modules, puis nettoie avec `anonymiser_json` |
| Diagnostic complet | `diagnostics()` (l. 788), `GET /system/diagnostics` ; `diagnostics_network` (SSDP, SlimProto, LMS) | JSON compact, sans secrets |
| Registre des passes | `task_runs` (`GET /system/task-runs`, #2080) | « ne contient ni chemin, ni clé, ni jeton » |
| Chemin du signal | `build_signal_path_lang` dans `tune-server/src/routes/zones/signal_path.rs` (l. 906) | **rend `None` quand la zone est arrêtée** : pour une zone qui ne joue pas, il manque précisément ce qu'on voudrait lire |
| Rapport de bug | `tune_core::bug_report::collect_system_info` + `generate_bug_report` | même nettoyage, destiné au forum |
| Nettoyage | `tune_core::confidentialite::anonymiser{,_avec,_json}` | la seule fonction de nettoyage du serveur (#5124) |

Point d'accroche proposé : une route du greffon,
`POST /api/v1/ext/assistant-ia/diagnostic {zone_id?, question?, langue?}`.
L'hôte lui fournit, via `HostServices`, une fermeture qui rassemble le
contexte. Le greffon n'a pas accès à `AppState`, et c'est voulu, comme pour
`criteres`. Le modèle reçoit ce contexte et répond selon un schéma fermé :

```json
{"constat": "…", "causes_probables": [{"cause", "indices": ["ligne ou champ cité"], "confiance"}],
 "a_essayer": [{"geste", "ou": "écran ou réglage"}],
 "rapport_utile": true, "resume_pour_le_rapport": "…"}
```

Le validateur rejette tout `indice` qui ne figure pas **mot pour mot** dans
le contexte envoyé : le modèle ne peut citer que ce qu'il a lu. Les `gestes`
proposés sont des textes, jamais des appels d'API. Quand un correctif touche
un réglage (rendre visible une zone masquée, changer de backend), l'écran
renvoie vers le réglage ; l'assistant ne l'exécute pas.

Le « rapport de bug pré-rempli » reprend `generate_bug_report` tel quel et y
ajoute le `resume_pour_le_rapport` du modèle sous un titre « Résumé de
l'assistant (IA) ». C'est l'utilisateur qui l'envoie, depuis l'écran Support
existant ; l'assistant ne l'envoie jamais lui-même.

### 1.3 Contexte et taille

| Bloc | Bornes proposées | Jetons estimés |
|---|---|---|
| Consigne et schéma | fixe | ≈ 800 |
| Zone visée : configuration, type de sortie, état, dernier chemin du signal connu | 1 zone | 1 000 à 2 000 |
| `/system/diagnostics` réduit (version, OS, sorties, découverte, passes en échec) | champs choisis | 2 000 à 3 000 |
| Journal : lignes `WARN`/`ERROR` et lignes qui citent la zone ou son appareil, sur les 30 dernières minutes | 200 lignes au plus | 8 000 à 14 000 |
| Question de l'utilisateur | 500 caractères | ≈ 150 |

Soit **15 000 à 20 000 jetons d'entrée**. La fenêtre de 1 000 lignes de
l'export complet coûterait trois à quatre fois plus, et noierait le modèle
dans les lignes des modules bavards. Il vaut mieux filtrer avant l'envoi.
Comme le chemin du signal disparaît à l'arrêt, il faut en garder **le
dernier connu** pour chaque zone. C'est un petit ajout au poller, et le seul
changement hors du greffon.

### 1.4 Données envoyées et vie privée

C'est la piste où le plus de données quittent la machine, et le nettoyage
actuel ne suffit pas. Il a été écrit pour le forum, et il **garde
volontairement** « les adresses IP du réseau local, les ports, les modèles »
et les noms annoncés par les appareils (en-tête de
`confidentialite.rs`). Pour un fournisseur tiers, il faut une passe
supplémentaire, appelée seulement par l'assistant :

- les **adresses IP** (v4 et v6) deviennent des jetons stables pour la
  question (`<ip-1>`, `<ip-2>`) : le modèle voit que deux lignes parlent du
  même appareil sans connaître l'adresse ;
- les **noms d'hôtes, de zones et d'appareils** sont pseudonymisés de la même
  façon (`<zone-A>`, `<appareil-2>`), et la table de correspondance reste
  **sur le serveur** : l'écran rétablit les vrais noms dans la réponse ;
- les **chemins** sont réduits à leur dernier élément utile (extension,
  `…/Musique/<dossier>`), en plus du `~` actuel ;
- les **jetons, clés et en-têtes** sont déjà masqués par `anonymiser`. La
  passe y ajoute les paramètres d'URL des services de streaming
  (`?token=`, `&sig=` sont déjà couverts, mais une signature à nom inhabituel
  ne l'est peut-être pas) ;
- les **titres et artistes** des pistes ne sont pas nécessaires au
  diagnostic, et sont remplacés par `<piste>`.

Avant le premier envoi, l'écran montre **le texte exact qui partira**,
dépliable, avec un bouton « Envoyer à <fournisseur> ». C'est la même logique
que l'aperçu du rapport de bug. Pour Ollama, l'écran le dit (« reste sur
votre réseau »), et l'aperçu peut rester replié.

### 1.5 Coût

Pour 20 000 jetons d'entrée et 3 500 jetons de sortie, réflexion comprise
(voir § 9.2) :

| Modèle | Entrée | Sortie | Total par question |
|---|---|---|---|
| Haiku 5.5 | 0,002 $ | 0,00175 $ | **≈ 0,004 $** |
| Sonnet 5.5 | 0,04 $ | 0,035 $ | **≈ 0,075 $** |
| Gemini 3.5 Flash-Lite (`couts.rs`) | 0,006 $ | 0,009 $ | ≈ 0,015 $ |

Avec le plafond par défaut de 2 €, cela fait environ 25 questions avec
Sonnet et environ 500 avec Haiku. Il faut donc **Haiku par défaut**, et
Sonnet sur demande pour un second avis. Le cache n'aide pas ici : le journal
change d'une question à l'autre.

### 1.6 Ollama

C'est faisable, et c'est même l'usage où le local a le plus de sens, car rien
ne sort. Mais 15 000 à 20 000 jetons dépassent le contexte par défaut de
beaucoup d'installations Ollama, et la requête du greffon (`/api/chat`)
n'envoie pas `options.num_ctx`. Le risque est que le début du contexte soit
**tronqué sans erreur**. C'est non mesuré. Il faudrait soit envoyer
`num_ctx`, soit réduire le journal à 60 lignes pour Ollama.

### 1.7 Risques

- **Hallucination** : une cause plausible mais fausse. Parade : des indices
  cités mot pour mot et vérifiés, une confiance affichée, et la formule
  « causes probables » plutôt qu'un verdict.
- **Action non voulue** : aucune, puisque la route ne fait que lire.
- **Fuite** : traitée au § 1.4. Le reste du risque tient à ce que la passe
  ne connaît pas (un nom de réseau Wi-Fi dans une ligne de journal, par
  exemple). D'où l'aperçu montré avant l'envoi.

### 1.8 Taille

Route, consigne, collecte filtrée, passe de pseudonymisation et ses tests,
validateur d'indices : **600 à 900 lignes** côté serveur, plus le dernier
chemin du signal connu (≈ 100 lignes), plus l'écran (un panneau sur la carte
de zone et dans Support).

### 1.9 Non établi

- La part des fils du forum dont la cause est vraiment lisible dans le
  journal. Pour la mesurer, il faudrait rejouer une dizaine de fils fermés
  avec leur journal joint (ces journaux sont derrière la connexion au forum).
- La qualité de Haiku 5.5 sur des journaux Tune réels.

## 2. Identités fausses

### 2.1 Valeur

Le fil 2394 du forum (Didier, 23/10/2026, rc7 sous Windows) : « le mythique
groupe de rock français Ange est remplacé par une charmante jeune femme, qui
ressemble peu à Christian Descamps ». La réponse a renvoyé vers #6463
(« Christophe », FabienM, fil 2302) : l'interface ne permet pas encore de
corriger le rattachement. #2221 (homonyme non musical accepté pour une
image) et #4805 (« le verrou est l'IDENTIFICATION ») montrent qu'il s'agit
d'une famille de problèmes, pas d'un cas isolé. Les noms d'un seul mot
(Ange, Christophe, Caravan) en sont les victimes typiques.

### 2.2 Existant et points d'accroche

- `tune-core/src/metadata/artistes_par_le_reseau.rs` : une recherche
  MusicBrainz, puis seulement les candidats qui **nomment** l'artiste local,
  puis un départage par la bibliothèque (un album local qui porte le titre
  d'un groupe de sortie du MBID). **Sinon rien n'est écrit**, et le cas est
  compté (`BilanReseau`). Cette règle est bonne : le modèle n'a pas à la
  remplacer, il peut intervenir **seulement sur les cas laissés sans
  réponse** (peloton serré, aucune confirmation).
- `tune-core/src/metadata/artist_enrichment.rs::resolve_mbid` : n'accepte
  qu'une correspondance exacte du nom, et c'est par là que passent les
  homonymes.
- Pochettes et portraits : la source est choisie par nom (Discogs, Deezer,
  etc., voir #2221) ; le signalement « Ne correspond pas » existe sur la
  fiche.
- Import de playlists : `tune-core/src/library/appariement_bibliotheque.rs`
  (service → bibliothèque, `classer_candidats`, seuil `MATCH_APPROX_SCORE`)
  et `tune-server/src/routes/playlist_manager.rs` (`/transfer`, `/import`).

Usage proposé, en trois volets :

1. **Départage d'un artiste** (`POST /artistes/{id}/identite`) : le modèle
   reçoit la discographie locale (titres d'albums et de pistes, années,
   labels) et 2 à 5 candidats MusicBrainz avec leur désambiguïsation
   (`disambiguation`, pays, type, dates, quelques titres de groupes de
   sortie). Il répond `{"choix": mbid | null, "raison", "confiance"}`. Le
   validateur exige que `choix` soit l'un des candidats fournis, et l'écran
   montre la proposition. L'écriture passe par
   `artistes_du_pressage::poser_les_mbid`, qui exige déjà une colonne vide et
   l'unicité, et l'opération entre au journal d'annulation. Le même geste
   doit permettre de **détacher** un MBID faux (le cas « Ange »), ce que ni
   l'écran ni l'API n'offrent aujourd'hui. Ce geste est un prérequis de
   #6463, IA ou pas.
2. **Contrôle visuel d'un portrait ou d'une pochette** : le modèle reçoit
   l'image candidate et le contexte (« groupe de rock français, années 70,
   5 membres »), puis répond `{"coherent": bool, "raison"}`. **Le greffon ne
   sait pas envoyer d'image** : `fournisseur::Demande` ne porte qu'un
   `systeme` et un `message` texte. Il faudrait une extension du type
   `Demande`, et un traitement par famille (bloc `image` d'Anthropic,
   `image_url` du dialecte OpenAI, `images` d'Ollama).
3. **Imports de playlists douteux** : pour les lignes sous le seuil
   d'appariement, le modèle reçoit la ligne source (titre, artiste, album,
   durée, ISRC) et les 5 meilleurs candidats locaux, puis répond par un
   indice de candidat ou `null`. Il ne peut **jamais** inventer de piste :
   seul un indice de la liste est accepté.

### 2.3 Données envoyées

Les noms d'artistes, les titres d'albums et de pistes de la bibliothèque, et,
pour le volet 2, l'image (un portrait public, une pochette). Ce n'est pas
sensible au sens du § 1.4, mais c'est **le contenu de la bibliothèque**, et
l'écran doit le dire une fois (« les titres de vos albums sont envoyés à
<fournisseur> »).

### 2.4 Coût

| Volet | Entrée / sortie | Haiku 5.5 | Sonnet 5.5 |
|---|---|---|---|
| Départage d'un artiste (discographie ≈ 1 500, 5 candidats ≈ 5 000) | 7 000 / 1 500 | ≈ 0,0015 $ | ≈ 0,029 $ |
| Lot de 100 artistes laissés sans réponse | — | ≈ 0,15 $ | ≈ 2,90 $ |
| Lot de 50 lignes d'import douteuses (≈ 400 jetons chacune) | 20 000 / 3 000 | ≈ 0,0035 $ | ≈ 0,07 $ |
| Contrôle visuel d'une image | dépend de la taille de l'image | non établi | non établi |

Pour la vision, la documentation Anthropic compte une image en fonction de
ses pixels. Un portrait réduit à 512 × 512 resterait de l'ordre de quelques
centaines de jetons, mais **ce chiffre est à relever** avant d'en faire une
promesse, de même que la prise en charge des images par Haiku 5.5 et Sonnet
5.5 (à lire dans `capabilities.image_input` de l'API Models).

### 2.5 Ollama

Le volet 1 et le volet 3 sont du texte court en JSON : faisables. Le volet 2
demande un modèle local à vision, et la qualité de ces modèles sur la
reconnaissance d'un artiste n'est pas établie.

### 2.6 Risques

- **Un modèle qui reconnaît une personne sur une photo** pose une question
  de principe : identifier des personnes réelles à partir de leur visage.
  Le volet 2 doit rester une **cohérence** (« cette image correspond-elle à
  un groupe de rock des années 70 ? »), jamais une identification (« qui
  est-ce ? »). Même dans ce cadre, je recommande de **ne pas l'activer par
  défaut** (§ 8).
- **Confirmation d'un faux** : le modèle peut choisir un candidat avec
  assurance. Parade : un choix borné aux candidats fournis, une confiance
  affichée, aucune écriture sans geste, une annulation possible, et le MBID
  posé par le modèle marqué `source_ia`, comme les champs d'album.

### 2.7 Taille

Volet 1 : 400 à 600 lignes, plus le geste « détacher le MBID », qui ne
dépend pas de l'IA (≈ 200 lignes). Volet 3 : 250 à 400 lignes. Volet 2 :
images dans `Demande` pour les quatre familles, plus le volet lui-même,
soit 500 à 800 lignes.

### 2.8 Non établi

- La cause exacte du cas « Ange » (un MBID faux, ou un portrait choisi par le
  nom seul sans MBID). #6463 n'est pas instruit.
- Le nombre d'artistes laissés sans réponse par `artistes_par_le_reseau` sur
  une vraie bibliothèque (le `BilanReseau` le compte, mais aucun relevé n'a
  été lu pour cette étude).

## 3. DJ qui explique / recherche en langage courant

### 3.1 Valeur

C'est moins un besoin exprimé qu'un agrément : on comprend pourquoi tel
titre suit tel autre. Le classement acoustique a déjà été critiqué sur ce
point (#1820 : le cosinus CLAP seul rapprochait des humeurs opposées). Une
ligne d'explication par transition rend le moteur lisible, et donc
corrigeable par l'utilisateur.

### 3.2 Existant et points d'accroche

- Le greffon `tune-dj` est un **mélangeur à deux platines** (charger,
  fondu, synchronisation du tempo, forme d'onde). Il ne choisit pas de
  pistes.
- Le choix des pistes vit ailleurs :
  `tune-core/src/ai/recommendations.rs` (`smart_radio`,
  `rerank_acoustic_queue`, `transition_penalty`, qui compare gain et BPM)
  et `tune-core/src/playback/auto_dj.rs` (`generate_queue`,
  `generate_mood_queue`, `tracks_for_artist_names`, avec BPM, genre et
  année). La recherche en texte libre par CLAP existe déjà
  (`tune-core/src/audio/text_embedding.rs`, branchée dans
  `routes/library/search.rs`), et la recherche conversationnelle aussi
  (`/ai/query`, § 11.5 du chantier).

Usage proposé : `POST /dj/expliquer {candidats: [track_id…], graine}`. Le
**moteur** fournit 15 à 25 candidats déjà filtrés (titres bannis exclus,
#4806), avec leurs traits (BPM, gain, genre, année, score acoustique). Le
modèle répond par un **ordre** et une phrase par transition :

```json
{"ordre": [12, 7, 33, …], "transitions": [{"de": 12, "vers": 7, "pourquoi": "…"}]}
```

Le validateur exige que l'ordre soit **une permutation d'un sous-ensemble
des candidats** : aucun identifiant inconnu, aucun doublon. Sinon, c'est
l'ordre du moteur qui s'applique. Les phrases sont affichées dans la file,
marquées IA.

### 3.3 Données, coût, Ollama

Ce qui part : des titres, des artistes et des traits numériques. Pour
20 candidats à environ 60 jetons, plus la consigne, on compte ≈ 2 500 jetons
d'entrée et ≈ 1 400 de sortie (10 lignes plus la réflexion) : **≈ 0,001 $
avec Haiku**, ≈ 0,019 $ avec Sonnet, par tranche de 10 titres. Sur une soirée
de 4 heures, environ 6 appels : moins d'un centime avec Haiku. Avec Ollama,
c'est faisable, car le texte est court et la permutation se vérifie.

### 3.4 Risques et validation

L'invention de pistes est impossible par construction (permutation
vérifiée). Le seul risque est une explication fausse (« même tonalité »
alors que la tonalité n'est pas fournie). La consigne interdit d'invoquer un
trait absent des données, et le validateur peut écarter une phrase qui cite
un trait non fourni (liste fermée de mots clés). Comme rien n'est écrit en
base, il n'y a rien à annuler.

### 3.5 Taille et non établi

300 à 500 lignes, plus l'affichage dans la file. Non établi : l'intérêt réel
des utilisateurs, car aucune demande du forum n'a été trouvée pour cette
étude.

## 4. Notes de pochette

### 4.1 Valeur

Les auditeurs de classique sont les premiers concernés. #4319 (« Mahler
symph 3 par Mehta non vu ») et #6481 (ouvrir le livret PDF, comme Audirvana
et jPlay) montrent l'attente : savoir quelle œuvre on écoute, de quels
mouvements elle se compose, et quelles autres versions on possède.

### 4.2 Existant et points d'accroche

- Les champs d'album et de piste, y compris ceux posés par l'assistant
  (`work`, `movement`, `composer`, `conductor`, `orchestra`, `performer` :
  `suggestions.rs`, `pistes.rs`).
- Les crédits MusicBrainz (`metadata/credits_mb.rs`, `credits_release.rs`),
  Wikidata (`metadata/wikidata.rs`) et la biographie de l'artiste
  (`metadata/bio_batch.rs::fetch_artist_bio`), qui porte sa source et sa
  licence (`BioResult.license`, CC BY-SA pour Wikipédia).
- Les autres enregistrements de la même œuvre dans la bibliothèque : une
  requête sur `track_metadata.work`.

Usage proposé : `POST /albums/{id}/notes {langue}`. Le contexte comprend
**uniquement** ce que la base contient déjà (champs, crédits, pistes avec
œuvre et mouvement, biographie et sa source, versions présentes). La
réponse suit ce schéma :

```json
{"paragraphes": [{"texte", "sources": ["champ:work", "bio:wikipedia", …]}],
 "oeuvre": {"titre", "mouvements": ["…"]} | null,
 "versions_presentes": [album_id…]}
```

Le validateur écarte un paragraphe sans source, ou dont une source ne fait
pas partie des blocs fournis, ainsi que tout `album_id` absent du contexte.
Le texte est rangé dans `album_metadata` (`notes_ia`, avec sa date et son
modèle), servi depuis le cache, affiché sous un bandeau « Écrit par l'IA à
partir de vos métadonnées » et effaçable.

### 4.3 Données, coût, Ollama

Ce qui part : des métadonnées publiques et la biographie, avec ≈ 4 000
jetons d'entrée et ≈ 2 200 de sortie, soit **≈ 0,0015 $ avec Haiku** et
≈ 0,03 $ avec Sonnet, que `couts.rs` propose déjà pour le classique
(`MODELE_ANTHROPIC_CLASSIQUE`). Le texte n'est produit qu'**une fois par
album** grâce au cache. Avec Ollama, c'est faisable ; la qualité de la
rédaction dépend du modèle.

### 4.4 Risques

- **Invention** : un modèle « sait » des choses sur Mahler et sera tenté de
  les écrire. La consigne l'interdit, le validateur exige des sources, et le
  bandeau IA prévient le lecteur. Le risque n'est pas nul pour autant : une
  phrase sourcée « bio:wikipedia » peut contenir un détail qui n'y figure
  pas. C'est non vérifiable automatiquement, d'où le bandeau.
- **Licence** : un texte tiré d'un résumé Wikipédia (CC BY-SA) en reste une
  œuvre dérivée. Il faut afficher l'attribution de la source, comme pour la
  biographie.

### 4.5 Taille et non établi

400 à 600 lignes, plus l'écran (un onglet « Notes » sur la fiche d'album).
Non établi : la part des albums classiques des testeurs qui ont déjà
`work` / `movement` remplis. Sans ces champs, les notes seront maigres.

## 5. Son sans danger

### 5.1 Valeur

Les questions sur le chemin du signal et l'égalisation reviennent sans
cesse (#6787 : « tout joue trop fort, il faut descendre vers 5 % »). Les
outils existent (modes de la pièce, AutoEq, préampli automatique), mais il
faut savoir lequel prendre. Un assistant qui **explique** le chemin du
signal et **oriente** vers le bon outil répond à ce besoin.

### 5.2 Existant et points d'accroche

- Le chemin du signal (`build_signal_path_lang`) : les étapes,
  `bit_perfect`, `preamp_db`, `peak_headroom_db`, `rate_conversion`, `pure`,
  avec les descriptions déjà traduites.
- Les modes de la pièce (PR #6823, ouverte vers le lot rc8) :
  `POST /api/v1/eq/room-modes` est **un calcul** qui rend 5 à 10 filtres en
  cloche **d'atténuation** entre 20 et 250 Hz. Rien n'y est appliqué, et
  l'application passe par `POST /zones/{id}/eq`.
- AutoEq (`tune-core/src/audio/autoeq.rs`, `/eq/autoeq/search`,
  `/eq/autoeq/import`) : des profils **mesurés**, qui peuvent contenir des
  bandes à gain **positif**, compensées par le préampli
  (`marge_de_tune_couvre_le_preamp`).

Usage proposé : `POST /son/conseil {zone_id, question}`. Le modèle reçoit le
chemin du signal (le dernier connu), l'égaliseur en place, et, si
l'utilisateur les a saisies, les dimensions de la pièce. La réponse suit un
schéma fermé :

```json
{"explication": "…",
 "orientation": "modes_de_la_piece" | "autoeq" | "aucune" | "verifier_le_niveau",
 "attenuations": [{"freq_hz", "gain_db", "q"}]}
```

**La règle absolue se tient dans le validateur, pas dans la consigne** :

- tout `gain_db` doit être **strictement négatif**, dans les bornes de la
  route des modes de la pièce (20 à 250 Hz). Une valeur nulle ou positive
  fait **rejeter la réponse entière**, sans correction silencieuse ;
- le modèle **ne propose jamais de profil AutoEq** par ses gains : il
  oriente vers l'écran AutoEq, où l'utilisateur choisit son casque. Les
  gains positifs d'un profil AutoEq sont une mesure publiée et compensée par
  le préampli de Tune, pas une invention du modèle ;
- **jamais de volume** : aucun champ de volume dans le schéma ;
- **rien n'est appliqué** : l'écran montre les atténuations dans l'éditeur
  paramétrique existant, et c'est « Appliquer » (`POST /zones/{id}/eq`) qui
  les pose, avec le préampli automatique et le fondu de bascule (#5215).
  L'annulation est celle de l'égaliseur (désactiver ou revenir au préréglage
  précédent).

En pratique, pour les modes de la pièce, le meilleur conseil est souvent de
**ne pas laisser le modèle calculer** : la route #6823 le fait par la
physique. Le modèle sert à expliquer et à orienter (« vos basses traînent :
mesurez la pièce et utilisez Modes de la pièce »), pas à remplacer le
calcul.

### 5.3 Données, coût, Ollama

Ce qui part : le chemin du signal (pseudonymisé comme au § 1.4 pour les
noms d'appareils), l'égaliseur et les dimensions. Environ 3 000 jetons
d'entrée et 1 500 de sortie, soit **≈ 0,001 $ avec Haiku** et ≈ 0,021 $ avec
Sonnet. Avec Ollama, c'est faisable.

### 5.4 Risques

Le risque principal, une hausse de niveau, est fermé par le validateur et
par l'absence de tout champ de volume. Reste un conseil inadapté (« coupez
à 63 Hz » sans mesure) : l'écran présente les atténuations comme des
**propositions à écouter**, et l'explication renvoie aux modes de la pièce
pour un calcul.

### 5.5 Taille et non établi

300 à 450 lignes, tests du validateur compris (les cas « gain nul »,
« gain positif » et « hors bande » rejetés). Non établi : le comportement de
#6823 une fois fusionnée ; l'étude s'appuie sur sa description de PR.

## 6. Commandes en langage courant (Home Assistant / voix)

### 6.1 Valeur

« Mets un peu de jazz calme dans la cuisine, pas trop fort. » Aujourd'hui :

- `POST /siri/voice-command` (`tune-server/src/routes/siri.rs`) reconnaît
  des **mots clés anglais** (`pause`, `next`, `back`…) ;
- `POST /voice-search` (`routes/voice.rs`) transcrit l'audio par Whisper
  avec `TUNE_OPENAI_API_KEY`, une variable d'environnement, **hors du
  coffre** et hors du plafond de l'assistant ;
- `/ai/query` sait déjà chercher, lancer, mettre en pause, changer de zone
  et régler le volume, avec les quatre fournisseurs.

Aucune demande du forum sur la voix n'a été trouvée pour cette étude. Côté
Home Assistant, la seule issue trouvée (#3809) concernait la découverte
Squeezebox.

### 6.2 Points d'accroche

- `voice_command` : quand aucun mot clé ne correspond, passer le texte à
  `Passerelle::discuter` (le dialogue à outils existant), au lieu de
  répondre « non compris ».
- Pour Home Assistant, le sens qui compte est **HA → Tune** : HA envoie le
  texte de son propre assistant vocal à la même route. Le module
  `routes/homeassistant.rs` va dans l'autre sens (Tune pilote HA) ;
  `ha_call_service` y transmet **n'importe quel** `domain/service` à HA.

### 6.3 Le volume : tenu par le serveur, jamais par le modèle

Ce qui est déjà acquis : l'outil `set_volume` de `/ai/query` passe par
`PlaybackOrchestrator::set_volume`, où les deux seuils de #6541 s'appliquent
(`volume_limites.rs`) : la **sécurité** borne tout, et le **confort** borne
les consignes absolues qui montent. Une demande « mets à fond » est donc
déjà ramenée sous le confort.

Ce qu'il faut corriger avant d'ouvrir la voix :

1. **Un argument absent vaut 0,5** (`executor.rs`, `set_volume` :
   `unwrap_or(0.5)`). Si la zone était à 20 %, un appel d'outil mal formé la
   fait **monter** à 50 % (sous le confort, donc permis). Un argument absent
   ou illisible doit être **refusé**, pas remplacé.
2. **Remplacer la consigne absolue par des pas** pour le modèle : un outil
   `volume_step {direction: "up"|"down", pas: 1..3}` qui passe par
   `POST /zones/{id}/volume/step` (le chemin des boutons + / −), plus un
   `set_volume` qui n'accepte que des valeurs **inférieures** au volume
   actuel. Une hausse reste possible, pas à pas, sous le confort et la
   sécurité, et chaque pas est une action visible dans la réponse
   (`actions`).
3. Un **plafond par commande** : au plus N pas vers le haut dans une même
   question (par exemple 3), quoi que le modèle demande.

### 6.4 Autres garde-fous

- **Jamais d'outil Home Assistant générique.** `ha_call_service` ouvre tout
  HA (serrures, alarme, chauffage). Si une piste HA se fait, les seuls
  outils exposés au modèle restent ceux de Tune (`tune_core::ai::tools`).
- Confirmation : la lecture et la pause n'en demandent pas, car elles se
  défont d'un geste. Un changement de zone qui coupe une autre écoute en
  cours devrait demander confirmation ; c'est à trancher.

### 6.5 Coût, latence, Ollama

Une commande compte 1 ou 2 tours, avec ≈ 1 500 jetons d'entrée (consigne,
outils, zones) et ≈ 300 de sortie : **≈ 0,0003 à 0,0006 $ avec Haiku**. À
50 commandes par jour, cela fait ≈ 0,50 à 0,90 $ par mois, dans le plafond.
Avec Sonnet (≈ 0,006 à 0,01 $ par commande), le même rythme coûterait 10 à
15 $ par mois et **dépasserait le plafond par défaut** : il faut donc Haiku,
ou Ollama. La latence d'un fournisseur distant (une à quelques secondes par
tour) n'a pas été mesurée ; pour la voix, c'est elle qui décidera. Avec
Ollama, le dialogue à outils est déjà traduit (`dialogue.rs`), donc c'est
faisable ; la fiabilité des appels d'outils par un petit modèle local n'est
pas établie.

### 6.6 Taille et non établi

Correctif du volume (points 1 à 3) : 150 à 250 lignes, **indépendant du
reste**. Repli de `voice_command` sur le dialogue : 150 à 250 lignes.
Transcription Whisper par le coffre : 200 à 300 lignes. Non établi : le
besoin réel, et le fait que HA sache envoyer son texte à une route HTTP
arbitraire sans intégration dédiée.

## 7. Bilan d'écoute et collections nommées

### 7.1 Valeur

« Mon écoute » existe (`routes/listening_stats.rs` : `/dashboard`,
`/wrapped`, `/heatmap`), et la PR #6820 y ajoute le temps d'écoute par
station de radio. Un bilan rédigé (« ce mois-ci, beaucoup de piano le soir,
et vous êtes revenu à Bill Evans ») et des collections **nommées** à partir
des grappes acoustiques CLAP (« Piano de nuit », « Cuivres du dimanche »)
rendraient ces chiffres vivants.

### 7.2 Existant et points d'accroche

- Les chiffres : `full_dashboard`, `wrapped`, et `radio_listening` (#6820).
  **`wrapped` est derrière `Feature::ListeningStats` (Premium)**, alors que
  l'assistant est gratuit. Un bilan IA fondé sur ces chiffres poserait la
  question de la porte : à trancher (§ 9).
- Les plongements CLAP sont rangés en base (`audio/embedding.rs`,
  `embedding_store`, 512 dimensions). Le regroupement (k-moyennes ou autre)
  se fait **localement, sans IA**. Le modèle ne sert qu'à **nommer** chaque
  grappe à partir de 10 à 15 titres représentatifs et de leurs genres.
- Les collections : le moteur des collections intelligentes existe ; une
  collection nommée par grappe pourrait être une collection classique
  (liste d'identifiants), proposée et jamais créée sans geste, comme la
  playlist par phrase.

### 7.3 Données, coût, Ollama

Pour le bilan : des agrégats (top 20 des artistes, des albums et des genres,
heures par moment de la journée, radios) et aucun horodatage précis, soit
≈ 5 000 jetons d'entrée et 2 000 de sortie : **≈ 0,0015 $ avec Haiku** par
bilan mensuel. Pour les noms de grappes : 20 grappes à ≈ 300 jetons, soit
≈ 6 000 jetons d'entrée et 1 500 de sortie : ≈ 0,0014 $. Avec Ollama, c'est
faisable.

Vie privée : les habitudes d'écoute sont **personnelles** (heures, moments
de la journée). Il faut n'envoyer que des agrégats, par profil, et rien
d'un autre profil que celui qui demande.

### 7.4 Risques et validation

Pour le bilan, le risque est un chiffre faux dans le texte. Le validateur
vérifie que chaque nombre cité figure dans les agrégats envoyés. Pour les
collections, le nom est libre, mais les membres viennent de la grappe
locale ; le modèle ne choisit pas les pistes. Rien n'est créé sans geste, et
une collection créée se supprime comme les autres.

### 7.5 Taille et non établi

Bilan : 250 à 400 lignes. Grappes et noms : 500 à 800 lignes, car le
regroupement local est le gros du travail. Non établi : la qualité des
grappes CLAP sur une vraie bibliothèque, et la part des testeurs dont
l'analyse acoustique est complète (elle est optionnelle et lente).

## 8. Recommandation ordonnée

### 8.1 Pour la rc8 (lot `batch/bugs-rc8-20261022`)

Uniquement ce qui ne fait que lire, ou qui corrige le socle :

1. **Correctif du volume de `/ai/query`** (§ 6.3, points 1 à 3) : argument
   absent refusé, pas plutôt que valeur absolue pour monter, plafond de pas
   par question. C'est un correctif, pas une fonction : il vaut avec ou sans
   les autres pistes.
2. **Les défauts ouverts de l'assistant d'abord** : #6731 (playlist
   enregistrée sans règle) et #6730 (motif d'erreur du fournisseur). Étendre
   un socle qui a deux défauts visibles n'est pas une bonne idée.
3. **Diagnostic, en version « explique-moi »** (§ 1) : lecture seule,
   pseudonymisation stricte, aperçu de ce qui part, rapport pré-rempli
   envoyé par l'utilisateur. C'est la piste la plus utile pour le forum et
   pour l'équipe. **À condition** que la passe de pseudonymisation et ses
   tests soient prêts ; sinon, après la v1.
4. **Notes de pochette** (§ 4) : lecture seule, sources obligatoires,
   bandeau IA, cache. Petit, et sans risque d'action.

### 8.2 Après la v1

5. **Identités, volet 1** (départage d'un artiste), avec le geste
   « détacher le MBID », qui ne dépend pas de l'IA et peut venir avant
   (#6463).
6. **Son sans danger** (§ 5), une fois #6823 fusionnée et éprouvée.
7. **Imports de playlists douteux** (§ 2, volet 3).
8. **DJ qui explique** (§ 3).
9. **Commandes en langage courant** (§ 6), après mesure de la latence.
10. **Bilan d'écoute et collections nommées** (§ 7), une fois la question
    Premium tranchée.

### 8.3 À ne pas faire

- **La vision envoyée par défaut** (§ 2, volet 2) : des images de personnes
  envoyées à un tiers, une identification de visages à éviter par principe,
  et un coût non établi. Au mieux, une option explicite, en cohérence et
  jamais en identification.
- **Toute hausse de niveau décidée par le modèle** : ni gain positif, ni
  volume absolu qui monte, ni profil d'égalisation inventé.
- **Un outil Home Assistant générique** exposé au modèle.
- **L'envoi des journaux complets** (1 000 lignes) à un fournisseur distant.
- **Un texte IA sans marque ni source** dans la bibliothèque.

## 9. Notes transverses

### 9.1 Un validateur par usage, comme aujourd'hui

Chaque piste reprend le contrat du greffon : un schéma fermé, une réponse
hors schéma qui rend `422 reponse_invalide`, et un modèle qui ne peut
désigner que des objets **fournis dans le contexte** (candidats, pistes,
albums, lignes de journal). Les écritures restent rares (MBID, collection,
notes), passent par un geste, entrent au journal d'annulation et portent
`source_ia`.

### 9.2 Réflexion et coût réel

Haiku 5.5 et Sonnet 5.5 réfléchissent par défaut (réflexion adaptative,
effort `medium` pour Haiku 5.5 et `high` pour Sonnet 5.5, selon la
référence de l'API Claude) ; les jetons de réflexion sont facturés comme de
la sortie. La requête Anthropic du greffon (`fournisseur.rs`) n'envoie ni
`thinking` ni `output_config.effort` : les estimations de sortie ci-dessus
comptent donc une marge de réflexion de 1 000 à 2 000 jetons. Envoyer
`effort: "low"` pour les usages simples (commandes, permutation du DJ)
réduirait le coût. C'est à mesurer sur des appels réels, que cette étude n'a
pas faits.

D'après la même référence, Haiku 5.5 compte aussi environ 30 % de jetons de
plus qu'Haiku 4.5 pour le même texte. Les chiffres ci-dessus sont des ordres
de grandeur, pas des mesures.

### 9.3 Cache de prompt

La consigne et le schéma de chaque usage sont fixes ; la mise en cache du
préfixe chez Anthropic (lecture à environ 0,1 fois le prix d'entrée) ne paie
que si le préfixe dépasse le minimum mis en cache et que les appels se
suivent à moins de cinq minutes. C'est le cas du lot de suggestions et du
lot d'identités, mais pas des usages ponctuels. Le greffon ne pose pas
`cache_control` aujourd'hui. C'est un gain possible sur les lots, non
mesuré.

### 9.4 Ollama

Partout, le texte court en JSON est faisable en local. Les deux limites sont
le contexte long du diagnostic (§ 1.6, `num_ctx` non envoyé) et la vision
(§ 2.5). Aucune mesure de qualité n'a été faite sur un modèle local.

## 10. Décisions pour Bertrand

1. **Ordre de la rc8** : correctif du volume, puis #6731 et #6730, puis le
   diagnostic et les notes de pochette (§ 8.1) ? Ou seulement le correctif
   du volume en rc8, et le reste après la v1 ?
2. **Diagnostic** : faut-il une pseudonymisation stricte (IP, noms
   d'appareils et de zones, titres) en plus de `anonymiser`, et un aperçu
   montré à chaque envoi ou seulement au premier ?
3. **Diagnostic** : le rapport de bug pré-rempli peut-il contenir le
   résumé de l'assistant, marqué IA, quand il part sur le forum public ?
4. **Volume par l'assistant** : pas seulement pour monter (outil
   `volume_step`), avec au plus 3 pas par question ? Ou aucune hausse du
   tout par l'assistant ?
5. **Vision** : à écarter, ou à proposer en option désactivée par défaut,
   limitée à la cohérence (jamais l'identification) ?
6. **Bilan d'écoute** : gratuit comme l'assistant, ou derrière la porte
   Premium de « Mon écoute » (`Feature::ListeningStats`) ?
7. **Voix** : faut-il faire passer la transcription Whisper
   (`TUNE_OPENAI_API_KEY`) par le coffre et le plafond de l'assistant ?
8. **Modèle par défaut** : Haiku 5.5 partout, Sonnet 5.5 seulement sur
   demande (second avis du diagnostic, notes du classique) ?
9. **Effort** : envoyer `effort: "low"` pour les usages courts (commandes,
   DJ) afin de réduire le coût de la réflexion ?
