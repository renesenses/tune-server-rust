# Étude : mise à jour automatique du binaire composite Diretta et licence par clé

*Étude du 14/10/2026. Pas de code de production. Base : `batch/bugs-rc5-20261012`.*

Point de départ : un partenaire veut installer Tune sur un appareil Linux sans
console ni SSH. Il demande deux choses : que l'appareil **se mette à jour seul**,
et que la licence **s'active par une commande scriptable**. L'appareil fait
tourner le **binaire composite**, c'est-à-dire le serveur Tune compilé avec la
sortie Diretta (dépôt privé, SDK Diretta sous licence). Il ne fait pas tourner
le binaire public `tune-server`.

Décisions de Bertrand (14/10) :

1. Tune se met lui-même à jour **vers le binaire composite**, par un canal de
   mise à jour séparé, avec des binaires signés publiés à chaque version, pour
   tous les utilisateurs du greffon.
2. Une **clé de licence porte le module `diretta`**. Elle s'active par
   `POST /api/v1/cloud/license/activate {"license_key": …}`.

Ce document décrit l'existant, ce qu'il faut ajouter, et les questions encore
ouvertes. Rien n'y décrit le code du greffon ni le SDK.

---

## (a) La mise à jour aujourd'hui, et le second canal

### Le chemin actuel (binaire public)

| Étape | Où | Ce qui se passe |
|---|---|---|
| Liste des versions | `tune-core/src/updater.rs` (`UpdateChecker::check`) | `GET https://mozaiklabs.fr/api/tune/releases?per_page=20` (proxy du site, sans plafond), puis repli sur l'API GitHub des releases de `renesenses/tune-server-rust`. La release retenue est la plus récente du canal (`UpdateChannel` : `auto` / `stable` / `beta`, réglage `update_channel`). |
| Choix de l'archive | `update.rs` `find_archive_asset` | La première archive `.tar.gz` ou `.zip` dont le nom commence par `tune-server` et qui correspond au système, à l'architecture et, sous Linux, à la libc (`cfg!(target_env = "musl")`). |
| Annonce | `GET /api/v1/system/update/check` → `update_release_payload` | `installable`, `asset_name`, `size_bytes`… `.no-auto-update` met `installable: false` (#6068). |
| Vérification périodique | `spawn_verificateur_de_mise_a_jour` (toutes les 6 h si `TUNE_AUTO_UPDATE=true`) | **Elle ne fait que noter qu'une version existe ; elle n'installe rien.** C'est délibéré : la garde anti-coupure (#2954) suppose qu'une installation est un geste volontaire. |
| Installation | `POST /api/v1/system/update/install` → `update_install` | Gardes dans l'ordre : Homebrew, puis **`.no-auto-update` à côté de l'exécutable ⇒ 409 `blocked`**, dossier non inscriptible, scan en cours, lecture en cours (sauf `?force=true`). |
| Téléchargement et vérification | `telecharger_actif`, puis `verify_update_signature` | L'archive est vérifiée **avant extraction**. On vérifie la signature minisign de `SHA256SUMS` (fichier `SHA256SUMS.minisig`, clé publique intégrée `UPDATE_PUBLIC_KEY`), puis le SHA-256 de l'archive dans ce `SHA256SUMS` authentifié. Une archive absente de la liste est refusée. |
| Remplacement (Unix) | `install_unix` | L'archive est extraite dans `tune-update-*`. Le binaire **nommé en dur `tune-server`** (ou `tune-server.exe`) y est cherché, copié en `<exe>.new`, puis l'exécutable en cours est renommé en `<exe>.old` et `.new` prend sa place. `web/` est remplacé. Si le renommage échoue, l'ancien binaire est remis en place. |
| Remplacement (Windows) | `install_windows` / `windows_update_bat` | Un script `.bat` fait l'échange après l'arrêt du processus. |
| Redémarrage | `relancer_le_serveur` | Sous Unix, `exec` sur place avec les mêmes arguments, différé tant qu'une zone joue (`defer_restart_until_quiet`). Sous macOS, relance par le lanceur du paquet. |
| Après redémarrage | `record_post_update_result`, `noter_la_version_vue` | `swap_took` compare la version attendue à la version qui tourne. `previous_version` garde la version d'où l'on vient (#2266). |

**Le retour arrière aujourd'hui.** `<exe>.old` survit à la mise à jour, mais
**rien ne le remet en place tout seul**. Aucune route ne sert à revenir en
arrière (#2266 reste ouvert : la migration de base à rebours est le vrai
problème). Si le nouveau binaire ne démarre pas, il faut une console pour s'en
sortir. C'est justement ce qui manque sur l'appareil du partenaire.

### Pourquoi le composite est bloqué aujourd'hui

Le composite appelle `tune_server::bootstrap::run_with(RunOptions { … })`, et
partage donc tout le code de mise à jour ci-dessus. Sans garde, cliquer
« Mettre à jour » **le remplacerait par le binaire public** : même numéro de
version, sortie Diretta disparue, aucun message. Le paquetage du composite pose
donc `.no-auto-update` à côté du binaire, et la mise à jour est entièrement
bloquée.

Deux défauts latents à corriger quel que soit le choix retenu :

- **Le préfixe `tune-server` attrape aussi `tune-server-<variante>-…`.** Si une
  archive composite était un jour publiée dans une release publique, les
  serveurs publics pourraient la choisir, selon l'ordre alphabétique (c'est le
  même mécanisme que l'incident du moissonneur du 17/09). Il faut un préfixe
  exact, `tune-server-v`, et le préfixe propre à la variante pour le composite.
- **Le nom du binaire extrait est écrit en dur.** Le composite doit chercher son
  propre nom dans l'archive.

### Ce qu'il faut ajouter : un canal « variante »

**1. Le binaire sait qu'il est composite, à l'exécution, par `RunOptions`.**

```rust
// tune-server/src/bootstrap.rs — proposition
pub struct RunOptions {
    pub build_plugins: Option<PluginBuilder>,
    pub output_providers: Vec<Arc<dyn OutputProvider>>,
    /// `None` = binaire public. `Some` = binaire composite : il se met à jour
    /// par SON canal, jamais par les releases publiques.
    pub variante: Option<VarianteBinaire>,
}
pub struct VarianteBinaire {
    pub id: &'static str,             // "diretta"
    pub nom_du_binaire: &'static str, // "tune-server-diretta"
    pub module_requis: Option<&'static str>, // "diretta"
    pub build: u32,                   // numéro de construction du composite, voir plus bas
}
```

Pourquoi pas une feature Cargo : le composite consomme `tune-server` comme une
dépendance, et une feature nommée d'après le partenaire dans le dépôt public
ne servirait qu'à lui. Un champ d'exécution est générique (il servira à toute
future variante), et il se teste dans ce dépôt (même logique que
`run_options_carry_output_providers`, #1510). La valeur est rangée dans un
`OnceLock` global, que `update.rs` lit sans toucher à `AppState`.

**2. Une seule bifurcation dans `update_check` / `update_install`.**

- `variante == None` : comportement actuel, sans changement (avec le préfixe
  exact corrigé).
- `variante == Some(v)` : on ne lit plus les releases GitHub, on interroge le
  **manifeste du canal composite** (point 3). Le reste du chemin est commun :
  téléchargement, vérification, extraction, gardes de scan et de lecture,
  `install_unix` / `install_windows`, relance, `swap_took`. Il cherche
  `v.nom_du_binaire` dans l'archive.
- **`.no-auto-update` garde son sens** : le fichier bloque, quelle que soit la
  variante. Les nouvelles archives composites ne le posent plus. Sur une
  installation qui l'a déjà, il faut le retirer une fois (installation manuelle
  de la première version qui a le canal), ou bien décider que le composite le
  retire lui-même au premier démarrage (voir les décisions).

**3. Le manifeste : servi par le site, réservé aux détenteurs du module.**

Le binaire composite **ne peut pas aller dans les releases publiques** : il
deviendrait téléchargeable par tous, et la licence du SDK l'interdit
probablement (question (b)). Il ne peut pas non plus venir des releases du
dépôt privé, que le serveur ne peut pas lire sans jeton. Le modèle existe déjà
sur le site : `GET /api/v1/audio-plugins/{plugin}/package?target=…` (paquets
signés de Crossfeed Pro). Il rend une URL **signée et temporaire**, la somme
SHA-256 et la signature minisign, après authentification par jeton SSO ou par
`X-License-Key`. Le pendant pour les composites :

```
GET {mozaik_base_url}/api/v1/composites/{variante}/latest
    ?target=x86_64-unknown-linux-gnu&channel=stable&current=1.0.0-rc5&build=3
Authorization: Bearer <SSO>   ou   X-License-Key: TUNE-XXXX-XXXX-XXXX-XXXX
→ 200 {
    "variante": "diretta",
    "version": "1.0.0",            // version de Tune embarquée
    "build": 1,                    // reconstruction du composite à version égale
    "target": "x86_64-unknown-linux-gnu",
    "asset_name": "tune-server-diretta-v1.0.0-b1-linux-x86_64.tar.gz",
    "url": "https://mozaiklabs.fr/…?signature=…&expires=…",   // 15 min
    "size": 48123456,
    "sha256": "…",
    "minisig": "untrusted comment: …\n…\ntrusted comment: tune-server-diretta 1.0.0 b1 x86_64-unknown-linux-gnu\n…",
    "notes_url": "…"
  }
→ 204 si rien de plus récent ; 403 module_required ; 404 no_package_for_target
```

Côté serveur, `update_release_payload` est rempli à partir de cette fiche, pour
que l'interface web et `/update/status` n'aient rien à apprendre. La fiche est
interrogée avec les mêmes identifiants que le catalogue des greffons
(`SupportAuth` : jeton SSO, puis clé de licence).

**4. Nom de l'asset par plateforme** (le même format que le public, avec le
préfixe de la variante, ce qui garde `find_archive_asset` réutilisable) :

| Plateforme | Asset |
|---|---|
| Linux x86_64 (glibc) | `tune-server-diretta-v{ver}-b{build}-linux-x86_64.tar.gz` |
| Linux aarch64 (glibc) | `tune-server-diretta-v{ver}-b{build}-linux-aarch64.tar.gz` |
| Windows x86_64 | `tune-server-diretta-v{ver}-b{build}-windows-x86_64.zip` |
| musl, macOS | seulement si le SDK les prend en charge (question pour le partenaire) |

L'archive contient `tune-server-diretta` (ou `.exe`) et le `web/` **de la même
version de Tune**.

**Le numéro `build`.** Le composite peut devoir être reconstruit à version de
Tune égale : correctif du greffon, nouvelle version du SDK. Le comparateur de
versions ignore les métadonnées semver `+…`. La précédence est donc
`(version, build)`, comparée telle quelle, et le serveur envoie son `build`
(constant du composite) dans la requête.

**5. Signature.** On garde minisign, déjà utilisé pour les releases et les
paquets de greffons (`minisign_verify` est déjà une dépendance). Deux choix sur
la clé (décision D3) :

- **Recommandé : une clé dédiée aux composites**, dont la moitié publique est
  intégrée au code public (`CLE_PUBLIQUE_COMPOSITES`, ce qui ne pose aucun
  problème) et la moitié secrète vit **uniquement** là où le composite est
  construit. On ne réutilise pas la clé des releases publiques : son secret
  est dans la CI publique, et il faudrait le copier sur une seconde machine,
  ce qui élargit son exposition. On ne réutilise pas non plus la clé des
  greffons audio : la compromettre suffirait alors à pousser un binaire
  complet, plus seulement un greffon.
- La signature porte **directement sur l'archive** (une seule archive par
  fiche, pas besoin de `SHA256SUMS`). Le **commentaire de confiance**, qui est
  signé chez minisign, doit valoir exactement
  `tune-server-diretta {version} b{build} {target}`. Le serveur le compare à la
  fiche. Sans cela, un site compromis pourrait resservir une ancienne archive
  authentique comme si c'était la nouvelle (attaque par rétrogradation), ou
  l'archive d'une autre plateforme.

**6. Installation sans personne devant l'écran.** C'est le point qui manque le
plus. Aujourd'hui, **aucune installation ne se fait sans geste humain** (voir
le tableau). Le partenaire demande que l'appareil se mette à jour seul. Il faut
donc un mode d'installation automatique, **réservé aux variantes ou réglable**
(décision D1) :

- un réglage `update_auto_install` (désactivé par défaut), ou bien activé
  d'office quand `variante.is_some()` ;
- le vérificateur de 6 h, quand il trouve une version, appelle le **même**
  chemin que `update_install` **sans `force`** : un scan en cours ou une zone
  qui joue retiennent l'installation, qui est retentée au tour suivant ;
- en option, une fenêtre horaire (par exemple 3 h–5 h, heure locale), pour ne
  jamais couper une écoute qui commence.

C'est une réouverture explicite de la garde #2954. Le commentaire de
`spawn_verificateur_de_mise_a_jour` dit que cet arbitrage revient à Bertrand.

**7. Retour arrière, indispensable sans console.**

1. **Essai à blanc avant l'échange** : lancer `<tmp>/tune-server-diretta
   --version` (le drapeau répond avant tout effet de bord, `bootstrap.rs`) et
   exiger la version annoncée. Cela attrape une bibliothèque d'exécution du SDK
   absente ou incompatible, une mauvaise architecture ou une mauvaise libc,
   **avant** de toucher au binaire en place. Ce gain vaut aussi pour le binaire
   public.
2. **Démarrage sous surveillance** : avant la relance, poser
   `.update-en-essai` (version attendue, compteur à 0) à côté du binaire. Au
   démarrage, le nouveau binaire incrémente le compteur et l'efface après 120 s
   en bonne santé. Au **3e** démarrage raté, le démarrage remet `<exe>.old` en
   place et relance l'ancien. Il faut qu'un superviseur relance le processus
   (systemd `Restart=on-failure`, ce qu'une appliance a de toute façon). Un
   retour arrière automatique est noté dans `last_update_result`
   (`rolled_back: true`) et se voit dans l'interface.
3. **La base de données** : le retour automatique n'est sûr que si la nouvelle
   version n'a pas encore migré la base, ou si ses migrations sont
   compatibles en arrière. Proposition : sauvegarde automatique de la base
   avant toute installation (`routes/system/backup.rs` existe), restaurée en
   même temps que `<exe>.old`. C'est le même nœud que #2266.

---

## (b) Qui compile et publie le composite, et où

**La contrainte.** Le SDK est sous licence personnelle, sans droit de
redistribuer le SDK lui-même. Le dépôt privé ne le contient jamais : chaque
testeur se procure aujourd'hui le SDK et **compile lui-même**. Distribuer un
**binaire compilé** avec le SDK est une autre question, qui n'est pas encore
tranchée. C'est la question n° 1 pour le partenaire.

**La cadence.** À chaque tag public `vX.Y.Z[-rcN]` de Tune, le composite doit
être reconstruit **sur ce tag** : faire pointer le composite sur le tag,
construire chaque plateforme, signer, puis déposer sur le site. Ce sont environ 3 archives par version.

**Trois façons de faire, sans trancher :**

| | CI privée (GitHub Actions du dépôt privé) | Machine dédiée (runner auto-hébergé ou script) | Le partenaire construit et publie |
|---|---|---|---|
| Déclencheur | `repository_dispatch` envoyé par `release.yml` au tag | idem, ou une veille sur les tags | notification du tag |
| Le SDK | archive **chiffrée** hors du dépôt, clé de déchiffrement en secret (un secret seul ne fait que 48 Ko) | installé une fois sur la machine, jamais copié | chez lui, sous sa propre licence |
| Clé de signature | secret de la CI | sur la machine seulement | la sienne, ou une étape de signature chez Mozaiklabs |
| Coût et risques | minutes Actions facturées pour un dépôt privé ; le SDK transite sur des machines de GitHub, ce que la licence autorise peut-être ou pas | une machine de plus à tenir ; la compilation Windows demande une machine Windows | dépendance à un tiers à chaque version ; la confiance dans le canal passe par sa clé |
| Plateformes | Linux x86_64 et aarch64 (cross ou runners arm), Windows | ce que la machine sait construire | ce qu'il prend en charge |

Dans tous les cas, l'archive est publiée sur le site par une commande sur le
modèle de la publication des paquets de greffons audio : stockage hors dépôt,
table de paquets, URL signée et temporaire. Elle n'est **jamais** publiée sur
GitHub.

**Questions à poser au partenaire (sans trancher) :**

1. La licence du SDK autorise-t-elle **la distribution d'un binaire compilé**
   avec le SDK (lié en statique ou en dynamique) aux acheteurs du module ? Avec
   quelles mentions, et à quelles conditions ?
2. Si le SDK est une bibliothèque dynamique : peut-on la livrer **à côté** du
   binaire dans l'archive, ou bien doit-elle venir de l'appareil (déjà présente
   sur son HostPC) ?
3. Le SDK peut-il être installé sur une **machine de CI**, éventuellement chez
   GitHub, ou seulement sur une machine que nous contrôlons ? À quel nom est la
   licence qui sert à la construction ?
4. Préfère-t-il **construire et signer lui-même** les binaires pour ses
   appareils ?
5. Son HostPC : architecture (x86_64 ou aarch64), distribution et libc (glibc
   ou musl), init (systemd ?), répertoire du binaire inscriptible par le compte
   de service, espace disque pour garder `.old` et une sauvegarde ?
6. Ce HostPC a-t-il un accès Internet **permanent**, intermittent, ou aucun ?
   (Cela décide à la fois la mise à jour et la licence, voir (c).)
7. L'image est-elle **clonée** d'un appareil à l'autre (même nom d'hôte, même
   machine-id) ? L'empreinte matérielle de la licence en dépend, voir (c).
8. Quelles plateformes et quelles versions du SDK faut-il couvrir, et à quel
   rythme le SDK change-t-il (ce qui déclenche des reconstructions `build+1`) ?
9. Une mise à jour automatique la nuit lui convient-elle ? Faut-il pouvoir la
   désactiver par commande ?

---

## (c) Une clé de licence qui porte ses modules

### L'existant

- **Serveur.** Les modules ne viennent que du **compte SSO** :
  `refresh_account_premium` (`background.rs`, battement d'une heure) et la
  liaison du compte (`routes/cloud.rs`) appellent
  `LicenseManager::set_modules(user.modules)`. Ce dernier **remplace** la liste
  et la persiste sous `mozaik_modules`. `clear_account_premium` les efface. La
  clé n'apporte que le palier, par `verdict_licence` → `update_from_server(tier,
  expires_at)`. La découverte des sorties hors arbre lit `license.modules()`
  toutes les 60 s (`spawn_output_providers`), et
  `ModuleRefusal::evaluate(possede, compte_lie)` laisse passer dès que
  `possede` est vrai : **un module venu de la clé suffira, sans compte lié**.
- **Site.** `module_purchases` (email, module, source `lemonsqueezy` ou
  `manual`, `order_id`, unique sur email+module). Elle est alimentée par le
  webhook LemonSqueezy (`order_created` dont le variant est un module : on
  enregistre l'achat **sans créer de licence**) et par `php artisan
  module:grant {email} {module}`. Seul `OAuthController` (userinfo) rend
  `modules`. `LicenseController::validate`, `activate` et le heartbeat
  (`routes/api.php`) **ne rendent pas de modules**. Les licences sont, elles
  aussi, rattachées à un email.

### Ce qu'il faut côté site

1. `LicenseController::validate` (cas valide) et la réponse du heartbeat
   (`license_valid: true`) ajoutent :
   ```php
   'modules' => \App\Models\ModulePurchase::modulesFor($license->email),
   ```
   Le champ est **toujours présent** dans une réponse valide (`[]` si aucun
   module) : son absence signifie « site ancien », pas « aucun module ».
2. **Une clé pour un acheteur du module seul.** Aujourd'hui, l'achat du module
   ne crée pas de licence. Il faut que le webhook crée (ou retrouve) une ligne
   `licenses` pour cet email, avec `tier = 'free'`, `plan = 'module'` et sans
   échéance, et envoie la clé par mail. Pour un partenaire ou une bêta, il faut
   un équivalent artisan (`module:grant --with-key`). Une clé `free` valide
   donne alors : palier Free, modules `["diretta"]`.
3. La session unique (`claimSession`) s'applique telle quelle : une clé, un
   appareil actif à la fois.
4. Tests Pest : clé avec et sans module, clé free plus module, heartbeat.

### Ce qu'il faut côté serveur

1. **Lecture.** `VerdictLicence::Confirmee` gagne `modules: Option<Vec<String>>`
   (`None` si le champ est absent). C'est la lecture unique du corps : le
   battement, la revalidation sans battement, `/license/validate` et
   `/license/activate` en héritent ensemble.
2. **Deux sources séparées, une union.** Il ne faut **pas** écrire les modules
   de la clé dans `mozaik_modules`. La prochaine relecture du compte (ou un
   compte lié sans le module) les effacerait, puisque `set_modules` remplace
   la liste. On ajoute `LicenseState.key_modules`, persisté sous
   `license_modules`, et :
   ```
   modules() = account_modules  ∪  (key_modules si la clé est active)
   ```
   « Clé active » se calcule comme `key_premium_active`, mais sans exiger le
   palier premium : la clé n'est pas expirée, et elle a été validée dans la
   fenêtre de grâce.
3. **Mise à jour.** `update_from_server(tier, expires_at, modules)` : si
   `Some(m)`, alors `key_modules = m`. Si `None`, on n'y touche pas. Un
   `Expiree` autoritaire vide `key_modules`. Un `RefusTransitoire` ou un
   conflit de session n'y touchent pas. `clear_license` (désactivation) les
   efface.
4. **Réponses.** `POST /cloud/license/activate` rend aujourd'hui
   `status: "pending"` dès que le palier n'est pas premium. **Une clé du module
   seul serait donc affichée « en attente » alors qu'elle est confirmée.**
   `status` doit dire `activated` dès que le verdict est `Confirmee`, et la
   réponse doit porter `modules`. `GET /cloud/license/status` ajoute `modules`
   et leur source (`account` ou `key`).
5. **Délai de grâce hors ligne.** La clé a 14 jours (`GRACE_PERIOD_DAYS`) après
   la dernière validation réussie. Les modules du compte **n'ont aujourd'hui
   aucune grâce** : persistés, ils restent tant que le compte ne les retire
   pas. Pour un module acheté une fois, à vie, perdre la sortie audio après
   14 jours sans réseau est rude. Décision D5 : même grâce que la clé (14 j),
   grâce plus longue pour les modules (par exemple 90 j), ou aucune expiration
   hors ligne pour un module à vie.
6. Tests : clé free plus module → la découverte voit le fournisseur ; relecture
   d'un compte sans module → le module de la clé reste ; expiration → il
   disparaît ; site ancien sans le champ → rien ne bouge.

### La commande pour le partenaire

Sur l'appareil (port local par défaut 8888 ; l'authentification locale est
désactivée par défaut, sinon il faut ajouter `-H "Authorization: Bearer …"`) :

```sh
curl -fsS -X POST http://127.0.0.1:8888/api/v1/cloud/license/activate \
  -H 'Content-Type: application/json' \
  -d '{"license_key":"TUNE-XXXX-XXXX-XXXX-XXXX"}'
# → {"status":"activated","tier":"free","modules":["diretta"],"expires_at":null,…}
```

Pour un script, le code de retour HTTP ne suffit pas : l'activation rend
**200 même quand la clé reste en attente** (site injoignable, conflit de
session). Il faut vérifier le corps :

```sh
curl -fsS -X POST http://127.0.0.1:8888/api/v1/cloud/license/activate \
  -H 'Content-Type: application/json' -d "{\"license_key\":\"$KEY\"}" \
| grep -q '"status":"activated"' || exit 1
# contrôle ultérieur :
curl -fsS http://127.0.0.1:8888/api/v1/cloud/license/status
```

La commande peut être rejouée sans risque (même clé, même appareil). La
sortie Diretta apparaît au tour de découverte suivant, en 60 s au plus, sans
redémarrage.

### Si l'appareil n'a pas Internet

- **Aujourd'hui : impossible.** La première validation en ligne est obligatoire
  (`set_license_key` pose la clé en attente, et le palier ne monte que sur une
  confirmation du site). Aucune clé ne débloque quoi que ce soit hors ligne.
  C'est voulu (c15dcc61).
- **Accès intermittent** : une activation en ligne suffit (par exemple en
  atelier, avant expédition), puis la grâce (D5) couvre les coupures.
- **Jamais d'accès** : il faudrait un **jeton de licence hors ligne**, signé
  par le site (minisign ou Ed25519, avec une clé dédiée) :
  `{license_key, modules, hardware_fingerprint, issued_at, expires_at}`.
  L'opérateur le génère sur le site à partir de l'empreinte lue sur l'appareil
  (`GET /cloud/license/status` rend `hardware_fingerprint`). Il l'importe par
  `POST /api/v1/cloud/license/offline {"token": "…"}`, que le serveur vérifie
  avec la clé intégrée. C'est une fonction nouvelle (décision D6). La
  **mise à jour** reste, elle aussi, impossible hors ligne : il faudrait une
  route d'envoi d'archive signée (`POST /system/update/upload`, vérifiée comme
  au point (a)5).
- **Empreinte et clonage** : l'empreinte est le SHA-256 du nom d'hôte et d'un
  identifiant de plateforme. Une image clonée sans régénérer ses identifiants
  donne la même empreinte à tous les appareils. La session unique traiterait
  alors des appareils différents comme un seul. C'est la question 7 pour le
  partenaire.

---

## (d) Étapes, taille, décisions ouvertes

### Étapes proposées

| # | Étape | Dépôt | Taille |
|---|---|---|---|
| 1 | Corriger le préfixe exact `tune-server-v` dans `find_archive_asset`, avec un test contre une archive `tune-server-<variante>-…` | public | S (½ j) |
| 2 | Site : `modules` dans `validate` et le heartbeat, clé `free`/`module` pour les achats du module seul, `module:grant --with-key`, tests Pest | site | S–M (1 j) |
| 3 | Serveur : `Confirmee.modules`, `key_modules` et l'union, `update_from_server`, réponses `activate` et `status`, grâce (D5), tests | public | M (1–2 j) |
| 4 | Serveur : `RunOptions.variante`, bifurcation de `update_check` et `update_install`, nom du binaire par variante, fiche du site, vérification minisign plus commentaire de confiance | public | M (2 j) |
| 5 | Site : `GET /api/v1/composites/{variante}/latest`, table des paquets, commande de publication, URL signée, contrôle du module | site | M (1–2 j) |
| 6 | Essai à blanc `--version` avant l'échange (vaut pour tous les binaires) | public | S (½ j) |
| 7 | Démarrage sous surveillance, retour automatique à `.old`, sauvegarde de la base avant installation | public | M–L (2–3 j) |
| 8 | Installation automatique (D1) : réglage, fenêtre horaire, sans `force` | public | S–M (1 j) |
| 9 | Chaîne de construction du composite au tag (selon (b)) : déclenchement, passage de la révision au tag, matrice, signature, dépôt sur le site ; retrait de `.no-auto-update` du paquetage | privé | M–L (2–4 j, selon la licence du SDK) |
| 10 | Essai de bout en bout sur un appareil sans console : activation par `curl`, mise à jour nocturne, retour arrière provoqué | — | S (1 j) |
| 11 | (Optionnel, D6) Jeton hors ligne et envoi d'archive signée | public + site | M (2 j) |

On peut commencer par les étapes 1 à 3 (licence) sans attendre les réponses
du partenaire. Elles répondent déjà à sa seconde demande, si l'appareil a
Internet. Les étapes 4, 5 et 9 dépendent de la question (b)1.

Total hors option : environ 12 à 18 jours de travail.

### Décisions encore ouvertes pour Bertrand

- **D1. Installation automatique.** On réouvre la garde #2954 : pour toutes les
  installations derrière un réglage, ou seulement pour les variantes ? Avec ou
  sans fenêtre horaire ?
- **D2. Où sont hébergés les binaires composites.** Le site, comme les greffons
  audio (recommandé), ou une autre solution ?
- **D3. Clé de signature.** Clé dédiée aux composites (recommandé), clé des
  releases, ou clé des greffons ? Qui garde le secret ?
- **D4. Qui construit** (CI privée, machine dédiée, ou le partenaire), sous
  réserve de ses réponses.
- **D5. Grâce hors ligne des modules.** 14 j comme la clé, une grâce plus
  longue, ou aucune expiration pour un module à vie ?
- **D6. Appareils jamais connectés.** Faut-il un jeton de licence hors ligne et
  une mise à jour par envoi d'archive, ou bien pose-t-on comme exigence au
  moins une connexion ?
- **D7. `.no-auto-update` sur les installations existantes.** Retrait manuel une
  fois, ou bien le composite le retire lui-même au premier démarrage s'il
  reconnaît le marqueur de son ancien paquetage ?
- **D8. Clé du module seul.** Créer une licence `free`/`module` à l'achat du
  module (et l'envoyer par mail), ou bien n'accepter le module par clé que
  pour les détenteurs d'une licence Premium ?
- **D9. Remboursement.** Aujourd'hui, un remboursement garde l'accès
  (`acces_conserve`). Faut-il appliquer la même règle au module, ou le retirer ?
