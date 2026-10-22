# Chantier « Assistant IA » — étapes 1, 2 et 3

GO de Bertrand du 12/10/2026. Fonction **gratuite**, pour tout le monde :
l'utilisateur fournit **sa** clé, les appels partent de **son** serveur vers
**son** fournisseur. Rien ne passe par mozaiklabs.fr.

Deux usages :

1. une **playlist intelligente décrite par une phrase** (« jazz calme des
   années 60 sans les live ») devient des **règles du moteur existant** de
   playlists et collections intelligentes ;
2. des **suggestions d'enrichissement sur un album**, depuis sa fiche
   (bouton « Suggestions IA »).

## 1. Greffon natif ou cœur ?

**Greffon natif in-tree** : `plugins/tune-assistant-ia`, un `TunePlugin`
comme `tune-circle` ou `tune-concerts`. Les points d'accroche existent tous :

| besoin | point d'accroche |
|---|---|
| routes HTTP | `PluginContext::register_router`, monté par l'hôte sous `/api/v1/ext/assistant-ia`, derrière `auth_middleware` |
| la base (réglages, albums, `album_metadata`) | `HostServices::backend` (`Arc<dyn DbBackend>`) |
| un dossier de données (cache) | `PluginContext::data_dir` |
| les critères du moteur des règles | `HostServices::criteres`, rempli par l'hôte depuis `tune_smart_http::criteres::criteres_exposes()` |

Le système de greffons **WASM** a été écarté : il n'offre ni l'accès à
`album_metadata` ni le tableau des critères, et un appel HTTP sortant vers un
fournisseur arbitraire n'y est pas une permission prévue. Le cœur aussi :
rien ici n'a besoin d'y vivre.

Comme tout greffon in-tree, il porte une feature, `assistant-ia`, **dans le
`default`** de `tune-server`, et il est inscrit à l'inventaire unique
`sdk/plugins.json` (`fast_test: true`) : les lignes de build, de test et de
clippy de `ci.yml`, `release.yml` et `docker.yml` en sont régénérées par
`python scripts/plugin-catalog.py --write`, donc il est livré dans tous les
binaires publiés. Il est chargé par défaut (`default_enabled`) : rien ne part
tant qu'aucun fournisseur n'est réglé.

## 2. Fournisseurs : une interface, une implémentation par famille

```rust
#[async_trait]
pub trait Fournisseur: Send + Sync {
    async fn completer(&self, demande: &Demande) -> Result<Reponse, ErreurFournisseur>;
}
```

| famille | point d'accès | authentification | modèle par défaut |
|---|---|---|---|
| `anthropic` | `POST {base}/v1/messages`, base `https://api.anthropic.com` | `x-api-key`, `anthropic-version: 2023-06-01` | `claude-haiku-5-5` ; `claude-sonnet-5-5` réglable pour le classique |
| `compatible_openai` | `POST {base}/chat/completions`, base `https://api.openai.com/v1` (Mistral : `https://api.mistral.ai/v1`…) | `Authorization: Bearer` | au choix de l'utilisateur |
| `gemini` | préréglage de `compatible_openai` : `https://generativelanguage.googleapis.com/v1beta/openai` | `Authorization: Bearer <clé Google AI Studio>` | `gemini-3.5-flash-lite` |
| `ollama` | `POST {base}/api/chat`, base `http://127.0.0.1:11434` | aucune | au choix de l'utilisateur |

`Fabrique` (une fermeture `Acces -> Box<dyn Fournisseur>`) est injectée dans
l'état des routes : la vraie en production, une fausse dans les essais.

### Gemini : préréglage compatible OpenAI, pas `generateContent`

Relevé le 12/10/2026 dans la documentation officielle
(<https://ai.google.dev/gemini-api/docs/openai>, page mise à jour le
02/09/2026) : adresse `https://generativelanguage.googleapis.com/v1beta/openai/`,
clé en `Authorization: Bearer`, modèle nommé sans préfixe `models/`. La page dit
la compatibilité « encore en bêta » et **ne documente pas**
`response_format: json_object` : ce champ n'est donc pas envoyé à Gemini (il
l'est aux autres points d'accès compatibles).

Ce n'est pas une perte : **aucune famille n'est crue sur parole**. Le JSON est
extrait de la réponse puis validé côté serveur contre le schéma (§ 5) ; une
réponse hors schéma rend `422 reponse_invalide`. Le mode JSON du fournisseur
n'est qu'une aide. D'où le préréglage plutôt qu'une implémentation dédiée de
`generateContent`, qui n'apporterait rien que la validation ne donne déjà.

Modèle par défaut : `gemini-3.5-flash-lite`, « fastest, most cost-effective »,
statut Stable, recommandé par Google pour un projet nouveau (page des modèles,
relevée le 12/10/2026).

## 3. La clé

- Saisie dans **Réglages ▸ Assistant IA** (`PUT /reglages {cle}`, `""` l'efface).
- **Chiffrée au repos.** Le mécanisme existant pour les jetons Qobuz/Tidal
  (`auth_tokens_<service>` dans `settings`) est **réutilisé** pour la
  classification : le réglage s'appelle `assistant_ia_api_key_chiffree`, son
  nom porte `api_key`, donc `tune_core::secrets::est_secret` le masque dans
  `/system/config`, le retire des exports et de la sauvegarde dans le nuage.
  Mais ces jetons sont en clair dans la base ; la clé d'API, elle, est en plus
  **scellée** : XChaCha20-Poly1305 (les primitives de `secret_envelope`) sous
  une clé maîtresse de 256 bits posée dans un **fichier à côté de la base**
  (`assistant-ia.cle`, droits 0600). Une copie de `tune.db` seule ne la livre
  plus. Fichier perdu : `cle_illisible`, l'écran demande de la ressaisir.
- **Jamais renvoyée** : l'écran ne reçoit que `cle_definie` et `cle_fin`
  (4 derniers caractères).
- **Jamais journalisée** : le module des fournisseurs ne journalise que la
  famille et le statut HTTP ; une erreur de transport est réduite à sa nature
  (délai, connexion) ; `Acces` a un `Debug` qui masque la clé.
- **« Tester la clé »** (`POST /tester`) fait un vrai petit appel et rend
  `{ok: true}` ou `{ok: false, error, message}` avec `error` parmi
  `cle_refusee` (401/403), `quota` (429, crédit épuisé), `surcharge` (5xx,
  529), `modele_inconnu`, `reseau`, `plafond_atteint`, `non_configure`,
  `tarif_inconnu`, `cle_illisible`. **Jamais 401** côté Tune : le client web y
  lit une session expirée et déconnecte.

## 4. Coût et plafond

- Chaque appel compte ses jetons d'entrée et de sortie (`usage` du
  fournisseur).
- Table des prix dans `couts.rs`, **seulement des tarifs documentés** :
  - Anthropic, relevé du 12/10/2026 (référence de l'API Claude) :
    `claude-haiku-5-5` 0,10 $ / 0,50 $ par million de jetons (requêtes de
    moins de 100 000 jetons), `claude-sonnet-5-5` 2 $ / 10 $ ;
  - Gemini, <https://ai.google.dev/gemini-api/docs/pricing> (page du
    09/10/2026, relevée le 12/10/2026) : `gemini-3.5-flash-lite` 0,30 $ /
    2,50 $, `gemini-3.1-flash-lite` 0,25 $ / 1,50 $. La page annonce des prix
    à peu près doublés au 01/01/2027 : **à relever avant**.
  - compatible OpenAI : **aucun prix inventé** — l'utilisateur saisit les deux
    prix de son modèle ; sans eux l'appel est refusé (`tarif_inconnu`) plutôt
    que compté pour zéro.
  - Ollama : 0.
- Les prix sont en dollars, le plafond en euros : taux `taux_usd_eur`
  réglable, **1 par défaut**, ce qui surestime la dépense tant que le dollar
  vaut moins d'un euro (choix de prudence, pas un taux relevé).
- **Plafond mensuel** réglable (`plafond_eur`, 2 € par défaut, un plafond
  illisible vaut 0). Consommation du mois dans `assistant_ia_consommation`,
  remise à zéro au changement de mois. Plafond atteint : **plus aucun appel**,
  `409 plafond_atteint` avec un message clair. La lecture du plafond, l'appel
  et le comptage tiennent sous un même verrou.
- Pas de garde Premium.

## 5. Cache

Clé : SHA-256 de (version des consignes, usage, famille, modèle, entrée
normalisée). L'entrée normalisée d'une playlist est la phrase en minuscules aux
espaces repliés **plus la consigne entière** (critères, genres de la
bibliothèque, langue) ; celle d'un album, son contexte complet. Une entrée par
fichier dans `data_dir/cache` (500 au plus). Une réponse du cache ne coûte
rien, même plafond atteint, et elle est **revalidée** à chaque lecture contre
l'état présent de la base.

## 6. Playlist par phrase

`POST /api/v1/ext/assistant-ia/playlist {phrase, cible?: "playlist"|"collection", langue?}`

Consigne : la liste **fermée** des critères du moteur
(`tune_smart_http::criteres`, le tableau unique #5547), avec, pour chaque
opérateur, la forme de sa valeur ; les genres les plus fréquents de la
bibliothèque, pour reprendre leur orthographe. Les familles qui désignent un
objet de la base par identifiant (`collection_ref`, `playlist_ref`,
`tag_ref`) et `credit` ne sont pas offertes au modèle.

Schéma de réponse demandé :

```json
{"name": "…", "match_mode": "all|any",
 "rules": [{"field": "…", "op": "…", "value": …}],
 "sort_by": "title|artist|album|year|duration|added_at|play_count|random|null",
 "sort_order": "asc|desc|null", "max_tracks": 1-10000 | null,
 "untranslated": ["ce que nulle règle n'exprime"]}
```

Validation serveur, règle par règle : critère connu du moteur **et** offert,
opérateur de sa famille, valeur de la bonne forme (`between` : deux bornes,
`in_last` : entier ≥ 1, dates `AAAA-MM-JJ` ou `now-Nd`…). Une règle inconnue est
**refusée** et rendue dans `refusees` avec sa raison (`critere_inconnu`,
`operateur_inconnu`, `valeur_invalide`). Réponse :

```json
{"ok": true, "cible": "playlist",
 "proposition": {"name", "match_mode", "rules", "sort_by", "sort_order",
                 "max_tracks", "untranslated", "refusees"},
 "depuis_cache": false, "cout_eur": 0.0004}
```

**Rien n'est créé** : l'écran montre les règles, l'utilisateur les modifie,
puis enregistre par la route existante `POST /api/v1/library/smart-playlists`.

## 7. Suggestions sur un album

`POST /albums/{id}/suggestions {langue?}` → `{suggestions: [{champ, avant,
apres, confiance, raison}], ecartees: [{champ, valeur, raison}]}`

Champs : `title`, `artist`, `genre`, `label` (colonnes de `albums`) ;
`subgenre`, `work`, `movement`, `orchestra`, `composer`, `conductor`,
`performer`, `lyricist`, `producer` (magasin `album_metadata` ; les cinq
derniers sont les clés « Crédits » du catalogue des champs). Schéma demandé :
`{"suggestions": [{"field", "value", "confidence": 0..1, "reason"}]}`.

**Garde-fous** (rejoués à l'application) :

- champ tenu à la main (`album_metadata.edition_manuelle`, C3) →
  `edition_manuelle` ;
- album identifié par MusicBrainz (`musicbrainz_release_id`) : tout champ déjà
  rempli → `musicbrainz` (remplir un vide reste permis) ; l'artiste dès que
  l'artiste porte un `musicbrainz_id` ;
- un changement de la seule casse de l'artiste → `renommer_l_artiste` (c'est un
  renommage de l'artiste, pour tous ses albums : geste de la fiche artiste) ;
- champ inconnu, valeur vide ou trop longue, valeur identique → écartés.

`POST /albums/{id}/appliquer {lignes: [{champ, valeur}]}` écrit par le chemin
de l'édition manuelle (`AlbumRepo::update`, `AlbumMetadataRepo::set`) — **base
seulement**, comme l'édition manuelle ; écrire dans les fichiers reste
`POST /library/albums/{id}/edition/write-tags`. Le marqueur d'édition manuelle
n'est **pas** posé : l'utilisateur a accepté, il n'a pas saisi. Les champs
acceptés sont nommés dans `album_metadata.source_ia` (« source : IA »).

**Annulable** : chaque application est une opération de
`album_metadata.assistant_ia_journal` (20 par album) avec la valeur précédente
de chaque ligne. `POST /albums/{id}/annuler {operation_id?}` restaure ; un champ
modifié depuis n'est pas écrasé (`modifie_depuis`). `GET /albums/{id}/historique`.

## 8. Essais (faux fournisseur, aucun appel réseau réel)

`plugins/tune-assistant-ia/tests/assistant.rs` et les essais de module :

- règle inconnue refusée (route et validateur, sur les **vrais** critères du
  moteur) ;
- plafond atteint qui bloque (playlist, album, test de clé) ; plafond 0 ;
  tarif inconnu refusé ;
- cache qui évite un second appel ;
- donnée MusicBrainz jamais proposée en écrasement, ni appliquée ; champ édité
  à la main jamais proposé ; appliquer puis annuler ;
- clé jamais dans une réponse d'API ni dans un journal : abonné `tracing`
  capturant tout, vrai client HTTP contre un faux « Anthropic » sur
  127.0.0.1 qui confirme recevoir la clé ;
- Gemini : vrai client contre un faux serveur local, `Bearer`, pas de
  `response_format`, coût au tarif de la table.

## 9. Étapes suivantes

**Étape 2** — livrée, voir § 10.

**Étape 3** (« continue », Bertrand, 13/10/2026)
- recherche conversationnelle dans la bibliothèque — décision du 13/10/2026 :
  **extension de l'existant**, livrée, § 11.5 ; radio « dans l'esprit
  de… » : pas pour l'instant ;
- suivi de consommation détaillé par usage, alerte à 80 % du plafond —
  livré, § 11.1 ;
- relevé des tarifs automatisé ou daté à l'écran, avant le changement de prix
  Gemini du 01/01/2027 — **daté à l'écran**, livré, § 11.2 ; le relevé
  automatisé reste ouvert (§ 11.3).

## 10. Étape 2 (GO de Bertrand du 12/10/2026)

### 10.1 Suggestions sur un lot d'albums

`POST /lot/suggestions {album_ids: [1…50], langue?, budget_eur?}` — un
**aperçu** : chaque album passe, UN PAR UN, par le chemin de l'album seul
(cache, plafond, garde MusicBrainz / édition manuelle, écart avant/après,
confiance). Rien n'est écrit.

```json
{"ok": true,
 "resultats": [{"ok": true, "album_id", "titre", "suggestions", "ecartees", "depuis_cache", "cout_eur"}
             | {"ok": false, "album_id", "error", "message"}],
 "arret": null | {"error": "plafond_atteint" | "budget_du_lot_atteint" | …, "message"},
 "non_traites": [ids], "cout_eur": 0.0}
```

- **Arrêt net** : à la première erreur d'APPEL (plafond mensuel atteint, clé
  refusée, quota, non configuré…) — elle frapperait l'album suivant de même —
  ou quand le coût du lot atteint `budget_eur`. Les albums restants sont
  rendus dans `non_traites`, aucun appel de plus ne part.
- Un album introuvable ou une réponse hors schéma n'arrête pas le lot.

`POST /lot/appliquer {albums: [{album_id, lignes}]}` applique album par album
(mêmes garde-fous qu'à l'unité, rejoués), et rend un rapport par album avec son
`operation_id`. `POST /lot/annuler {operations: [{album_id, operation_id?}]}`.

### 10.2 Suggestions sur les pistes

`POST /albums/{id}/pistes/suggestions` → `{suggestions: [{piste_id, titre,
champ, avant, apres, confiance, raison}], ecartees}` pour l'**œuvre** (`work`)
et le **mouvement** (`movement`), rangés dans `track_metadata`.

Aucun écran ne saisit encore à la main l'œuvre d'une piste : il n'existe pas
de marqueur d'édition manuelle à lire. La garde est donc plus stricte que sur
l'album : **l'assistant ne remplit que des vides**, ou remplace une valeur
qu'il a lui-même posée (`track_metadata.source_ia`). Toute autre valeur est
tenue (`deja_rempli`) ; sur un album identifié par MusicBrainz ou une piste
qui porte un `musicbrainz_recording_id`, un champ rempli est tenu même posé par
l'assistant (`musicbrainz`). Une piste qui n'est pas de l'album :
`piste_inconnue`.

`POST /albums/{id}/pistes/appliquer {lignes: [{piste_id, champ, valeur}]}` ;
l'opération entre au journal de l'album (lignes avec `piste_id`) et s'annule
par `POST /albums/{id}/annuler`.

### 10.3 Le genre accepté vaut pour les pistes

Comme l'édition manuelle (#5314, `tune_core::db::genre_album_pistes`) : un
genre accepté est recopié sur les pistes (`tracks.genre`, `tracks.genres`,
`albums.genres`, marqueur `genre_pistes`), sauf compilation aux genres de
pistes différents (`compilation_epargnee`). L'opération garde, piste par
piste, la valeur d'avant et celle d'après (`genre_pistes` dans le journal).
L'annulation rend chaque piste dont le genre est encore celui de la recopie
(`pistes_genre`), laisse celles retouchées depuis (`pistes_modifiees_depuis`),
et restaure `albums.genres` et le marqueur. Le genre des pistes n'est rendu
que si celui de l'album l'a été.

### 10.4 Étiquettes et collections dans la playlist par phrase

Le moteur expose `tag` (`tag_ref`) et `in_collection` (`collection_ref`). La
consigne donne au modèle la liste des **noms** existants (200 au plus par
famille) : étiquettes (`tags`), collections classiques (réglage
`collections`, la source même du résolveur du moteur) et intelligentes
(`smart_collections`). Le modèle répond par un nom ; la validation le traduit
en identifiant moteur (`7`, `classic:3`, `smart:5`), sans tenir compte de la
casse. Nom inconnu : `reference_inconnue` ; porté par deux objets :
`reference_ambigue`. Sans étiquette (ou collection) dans la base, le critère
n'est pas offert. `in_playlist` (`playlist_ref`) et `credit` restent hors de
l'assistant.

### 10.5 Migration de l'ancien `/ai/query` et de la traduction des ambiances

`tune_assistant_ia::migration` : les clés EN CLAIR `anthropic_api_key`,
`gemini_api_key`, `openai_api_key` (dans cet ordre, celui de l'ancienne
traduction) passent au coffre :

- **adoptée** si l'assistant n'a pas de clé et que son fournisseur est vide
  ou le même — le fournisseur est alors posé ;
- **déjà présente** si l'assistant a la même ;
- sinon **conservée à part**, scellée sous
  `assistant_ia_api_key_heritee_<famille>` (rien n'est perdu).

La valeur en clair n'est effacée qu'après relecture de la valeur scellée et
vérification qu'elle s'ouvre sur la même clé. Idempotente ; sans clé héritée,
rien n'est écrit, pas même la clé maîtresse. Jouée au démarrage du greffon, et
avant chaque usage hérité (une clé reposée en clair par un ancien client est
rattrapée). `anthropic_model` n'est pas repris (pas de tarif documenté).

`tune_assistant_ia::Passerelle`, construite par l'hôte
(`plugins::passerelle_assistant`) sur la même base, le même dossier de clé et
**le même verrou de consommation** (`appel::verrou_de_consommation`, unique
pour le processus) :

- `POST /ai/query` : chaque tour du dialogue à outils passe par
  `discuter_anthropic` — clé du coffre, modèle et adresse de l'assistant,
  comptage au tarif de la table, refus plafond atteint (réponse
  `{reply, error: "plafond_atteint"}`, jamais 401). Il faut que le
  fournisseur de l'assistant soit Anthropic (le pilotage par outils est écrit
  pour l'API Messages) ; sinon, repli local, comme sans clé ;
- la traduction des ambiances (`translate_query_avec`) part chez le
  fournisseur de l'assistant, quel qu'il soit, sans mode JSON, comptée et
  bornée par le plafond ; `translation_available` vaut « assistant prêt, ou
  clé héritée en attente de migration ».

Un binaire compilé sans `assistant-ia` garde le chemin historique.

### 10.6 Réglages réservés aux administrateurs

`PUT /reglages` et `POST /tester` (qui dépense) suivent la règle de
`RequireAdmin` : sans authentification, ouverts ; avec, un jeton
d'administrateur (`AuthUser.role == "admin"`), sinon `403 admin_requis` —
jamais 401. `GET /reglages` reste lisible et dit `modifiable`. Les usages
(playlist, suggestions) restent ouverts à tous les profils. Le client web
n'affiche le formulaire qu'à un administrateur.

### 10.7 Essais (faux fournisseur, aucun appel réseau réel)

`plugins/tune-assistant-ia/tests/etape2.rs`, les essais de module
(`migration`, `pistes`, `suggestions`, `regles`) et
`tune-server/tests/assistant_ia_ancien_assistant_etape2.rs` (la route montée
`/ai/query`, plafond à zéro).

## 11. Étape 3

### 11.1 Consommation par usage, alerte à 80 %

La consommation du mois (`assistant_ia_consommation`) porte, en plus des
totaux, `par_usage` : les mêmes compteurs (coût, appels, jetons, réponses du
cache) pour chaque usage compté — `playlist`, `album` (l'album seul et le
lot), `pistes`, `test`, `requete` (l'ancien `/ai/query`), `traduction`. Une
consommation rangée avant l'étape 3 se relit (champ absent : vide). La
somme des usages refait le total.

`couts::alerte` : la dépense a atteint 80 % du plafond (un plafond nul est
atteint d'emblée). L'état du plafond (`depense_eur`, `plafond_eur`, `part`,
`seuil_alerte`, `alerte`, `atteint`) est joint, sous `plafond`, à chaque
réponse d'un usage (`/tester`, `/playlist`, `/albums/{id}/suggestions`,
`/albums/{id}/pistes/suggestions`, `/lot/suggestions`) ; `GET /reglages` le
donne dans `consommation` (`alerte`, `part_du_plafond`, `seuil_alerte`,
`par_usage`). L'appel qui franchit le seuil le journalise une fois
(`assistant_ia_alerte_80_pour_cent`, montants seulement). L'alerte ne bloque
rien : seul le plafond refuse.

Écran : un encart sous chaque usage (playlist par phrase, suggestions sur un
album, sur ses pistes, sur un lot) et dans Réglages ▸ Assistant IA tant que
le plafond n'est pas atteint ; le détail par usage y est replié.

### 11.2 Tarifs datés

`couts::Releve` date chaque table : `RELEVE_ANTHROPIC` (12/10/2026) et
`RELEVE_GEMINI` (12/10/2026, source <https://ai.google.dev/gemini-api/docs/pricing>,
à relever avant le 01/01/2027). `GET /reglages` rend `tarif` : `origine`
(`table`, `saisi`, `local`, ou `null` = tarif inconnu), les deux prix,
`releve_le`, `source_url`, `a_relever_avant`, et `a_relever` (la date
annoncée est passée). L'écran affiche le tarif appliqué et sa date, annonce
le changement à venir, et, la date passée, prévient que la dépense comptée
peut être sous-estimée et invite à saisir les nouveaux prix (qui priment sur
la table) ou à mettre Tune à jour.

### 11.3 Laissé à Bertrand

- **Recherche conversationnelle** : tranché le 13/10/2026 — extension de
  l'ancien `/ai/query` à tous les fournisseurs, bibliothèque seule (§ 11.5).
  **Radio « dans l'esprit de… »** : pas pour l'instant.
- **Après le 01/01/2027** : tranché le 13/10/2026 — compter à l'ancien tarif
  et prévenir (ce qui est fait).
- **Relevé automatisé des tarifs** : aucun fournisseur n'expose ses prix par
  une API ; un relevé automatique voudrait dire lire leur page HTML, ou un
  fichier publié par Tune. Non fait.

### 11.4 Essais

`plugins/tune-assistant-ia/tests/etape3.rs` (faux fournisseur) et les essais
de module de `couts` et `reglages` ; côté web,
`src/lib/__tests__/assistantIaEtape3.test.ts`.

### 11.5 Le dialogue de l'ancien assistant, pour tous les fournisseurs

Décision de Bertrand du 13/10/2026 : « extension de l'existant ». L'ancien
`POST /ai/query` (dialogue à outils : chercher dans la bibliothèque, jouer
un album ou une piste, file d'attente, pause, volume, zones) n'exigeait
qu'Anthropic ; il part désormais chez le fournisseur de l'assistant **quel
qu'il soit**.

- `Passerelle::discuter` : l'hôte écrit toujours le dialogue dans la forme de
  l'API Messages ; `tune_assistant_ia::dialogue` le traduit — compatible
  OpenAI et Gemini : `POST {base}/chat/completions`, outils
  `type: function`, appels dans `tool_calls` (arguments en texte JSON),
  résultats en messages `role: tool` + `tool_call_id` ; Ollama :
  `POST {base}/api/chat`, mêmes outils, arguments en objet, résultats
  rattachés par `tool_name`, identifiants d'appel fabriqués (`appel_N`). La
  réponse revient en blocs `text` / `tool_use`. Arguments illisibles : `{}`.
- `Passerelle::dialogue_pret` remplace `anthropic_pret` côté hôte : tout
  fournisseur prêt (clé qui s'ouvre, tarif connu). Sinon, repli local comme
  avant.
- Chaque tour passe par le même chemin (`Passerelle::tour`) : clé du coffre,
  plafond sous LE verrou, comptage pour l'usage `requete` (§ 11.1, donc
  détail par usage et alerte à 80 %). La réponse porte `plafond`, comme les
  autres usages ; plafond atteint : `{reply, error: "plafond_atteint"}`,
  jamais 401.
- **Coût exact de la question** (décision de Bertrand du 13/10/2026) : la
  réponse porte `cout_eur`, la somme de ce qui a été compté à CHACUN de ses
  tours (`Passerelle::discuter_compte` rend le coût du tour, au tarif daté et
  au taux de l'assistant, celui-là même qui entre dans la consommation). Ce
  n'est ni la dépense du mois ni une différence avant/après : un autre usage
  lancé pendant la question n'y entre pas. Un refus en cours de question
  (plafond atteint au 2ᵉ tour…) porte aussi `cout_eur` et `plafond` : les
  tours déjà payés y restent. Le repli local et le chemin historique (binaire
  sans le greffon) ne comptent rien et n'ont ni `cout_eur` ni `plafond`.

```json
{"reply": "…", "actions": [{"tool", "input", "result"}], "zone_id": 1,
 "cout_eur": 0.0012,
 "plafond": {"depense_eur", "plafond_eur", "part", "seuil_alerte", "alerte", "atteint"},
 "error": "plafond_atteint" | "cle_refusee" | "erreur_fournisseur" | …}
```
- **Bibliothèque seule** : les outils sont ceux de `tune_core::ai::tools`,
  tous locaux (aucun service de streaming).

**Ce qui part au modèle, borné** (`tune-server/src/routes/ai.rs`) — c'est ce
qui fixe le coût d'une question :

| borne | valeur |
|---|---|
| un résultat d'outil | 6 000 caractères (≈ 1 500 jetons), tronqué au-delà |
| `search_library` | 10 artistes, 10 albums, 10 pistes (inchangé) |
| les résultats d'une question | 24 000 caractères en tout (≈ 6 000 jetons) ; au-delà l'outil s'exécute mais le modèle reçoit `limite_de_bibliotheque` |
| les tours | 5 tours d'outils + 1 tour final (inchangé) |
| la sortie | 1 024 jetons par tour (Anthropic ; les points d'accès compatibles n'acceptent pas tous ce champ) |

Pire cas d'une question : 6 appels ; à chacun la consigne et les outils
(≈ 1 000 jetons) plus les résultats déjà reçus (≤ 6 000 jetons), soit
≈ 40 000 jetons d'entrée et ≈ 6 000 de sortie — de l'ordre de 0,007 $ avec
Claude Haiku 5.5, 0,03 $ avec Gemini 3.5 Flash-Lite, aux tarifs du § 4. Une
question ordinaire (une recherche, un lancement) en coûte une petite
fraction : 2 ou 3 appels de 1 000 à 3 000 jetons.

Essais : `plugins/tune-assistant-ia/tests/dialogue.rs` (faux serveurs
compatible OpenAI et Ollama sur 127.0.0.1 : traduction aller-retour, clé
`Bearer`, comptage `requete`, alerte, refus au plafond sans requête), les
essais de module de `dialogue`, et `tests_bornes` dans `routes/ai.rs`.

Essai du coût exact : `tune-server/tests/assistant_ia_cout_exact.rs` (route
montée, faux fournisseur : deux questions de même coût, la seconde rend SON
coût et non la dépense du mois ; refus au plafond au 2ᵉ tour, le 1ᵉʳ tour
payé reste dans `cout_eur`).

Client web : le panneau « Demander à l'assistant » (barre du haut, visible
seulement si l'assistant est prêt) appelle `/ai/query` — tune-web-client#2182,
`src/components/v2/DemanderAssistantIaV2.svelte`. Il affiche les liens lire /
ajouter tirés de `actions`, le coût de la question (`cout_eur`) et l'état du
plafond (`plafond`). Face à un serveur antérieur sans `cout_eur`, il n'affiche
que la dépense du mois et le plafond.
