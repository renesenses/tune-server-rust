# Spec / TODO — SSO « Se connecter avec mozaiklabs.fr »

> Statut : **non démarré**. Le login SSO est actuellement dégradé gracieusement
> (« Cloud bientôt disponible », PR #357). Ce document décrit le chantier pour
> l'activer proprement.
>
> **Ce document est un brief autonome** : il contient tout le contexte nécessaire
> pour être exécuté par un agent/développeur qui ne connaît pas l'historique.

## 0. Contexte opérationnel (à lire en premier)

**Repos & branches**
- `tune-server-rust` (Rust : `tune-core`, `tune-server`) — backend. `tune-web-client` (Svelte 5) — front, **repo séparé**.
- **Les releases sont taguées sur `main`** (branche v0.8.x). `release/v0.9` = refonte future non publiée. → Un fix doit atterrir sur `main` pour être livré. Certains fichiers (ex. `poller.rs`) ont divergé entre les deux branches ; `sso.rs`/`cloud.rs` sont normalement alignés.
- Discipline : **jamais de commit direct sur `main`** — toujours une branche `fix/…` ou `feat/…` → PR → `gh pr merge --squash --admin` → `git checkout main && git fetch origin && git merge --ff-only origin/main`.

**Build & test**
- Compiler : `cargo check -p tune-server` (features locales) ; `cargo check -p tune-core`.
- Tests : `cargo test -p tune-core --lib cloud::sso` (module SSO). Ajouter des tests unitaires (le module a déjà `authorize_url_format`).
- Format **obligatoire avant commit** : `cargo fmt` puis `cargo fmt -- --check` (la CI a un check Format qui casse le tag sinon).
- Web : `npm ci && npm run build` dans `tune-web-client` (produit `dist/`).

**Pièges connus**
- **GitHub secret-scanning push protection** est actif : tout `client_secret`/`GOCSPX-…`/pattern de secret dans un commit **bloque le push**. → Raison de plus pour le design **PKCE public sans secret** (§3). Ne jamais commiter de secret en clair.
- **Contrainte produit absolue** : Tune doit marcher **100 % sans mozaiklabs.fr**. Le SSO est **opt-in**, jamais bloquant, dégrade gracieusement si non configuré (déjà en place, ne pas casser).
- Client web : `apiFetch(path)` est **GET-only** (ignore tout 2e argument). Pour POST/PATCH utiliser `apiPost`/`apiPatch`.

**Décisions par défaut recommandées (pour ne pas bloquer le démarrage)** — à faire confirmer par Bertrand mais on peut avancer dessus :
- Redirect : **Option A (loopback 127.0.0.1 port variable, RFC 8252)**.
- **Client public + PKCE**, aucun secret distribué.
- Périmètre lot 1 : **identité seule** (se connecter + afficher le profil). Cloud sync / premium = lot 2.
- Le SSO **ne remplace pas** la licence pour l'instant (systèmes séparés, articulation à décider en lot 2).

## 1. Objectif

Permettre à un utilisateur de lier son installation Tune (auto-hébergée) à son
compte **mozaiklabs.fr**, via OAuth2, pour débloquer les fonctions Cloud/Premium
(cloud sync, bridge, premium tier…).

Tune = **client** OAuth. mozaiklabs.fr = **serveur d'autorisation** (Laravel Passport).

## 2. Ce qui existe déjà (à ne pas refaire)

Côté Tune (déjà codé, flux Authorization Code) :
- `tune-core/src/cloud/sso.rs` — `MozaikAuth::authorize_url()` → `{base}/oauth/authorize?client_id&redirect_uri&response_type=code` ; échange token → POST `{base}/oauth/token` (avec `client_secret` ⚠️).
- `tune-server/src/routes/cloud.rs` — routes `/cloud/sso/authorize`, `/cloud/sso/callback`, `/cloud/sso/status`. Lit `mozaik_client_id` (settings ou env `TUNE_MOZAIK_CLIENT_ID`), `mozaik_base_url` (défaut `https://mozaiklabs.fr`), `redirect_uri` (défaut `http://localhost:{port}/api/v1/cloud/sso/callback`). Stocke `mozaik_access_token`, `mozaik_user`.
- Client web : bouton « Se connecter », `/cloud/sso/status` → `cloudSsoConfigured`. Dégradation gracieuse si non configuré (#357).

**Manque uniquement** : le client OAuth côté mozaiklabs.fr + le `mozaik_client_id` dans Tune + le passage en PKCE + la gestion du redirect localhost.

## 3. Décision d'architecture : client PUBLIC + PKCE (pas de secret)

Tune est **distribué** (chaque utilisateur a le binaire). Un `client_secret` embarqué
n'est pas secret → même faille que le secret YouTube. **On bascule en client public
+ PKCE** (RFC 7636) : pas de secret, un `code_verifier`/`code_challenge` par flux.

- Passport : créer un client **public** (`--public`, sans secret) avec PKCE activé.
- Tune : supprimer `GOOGLE_CLIENT_SECRET`-style ; générer `code_verifier` (aléatoire),
  envoyer `code_challenge=S256(verifier)` à `/oauth/authorize`, renvoyer `code_verifier`
  à `/oauth/token`.

## 4. Le problème du `redirect_uri` localhost (le vrai point dur)

Chaque Tune écoute sur `http://localhost:<port>` (port variable, ou IP LAN). Passport
matche le `redirect_uri` **exactement**. Trois options :

**Option A — Loopback à port variable (RFC 8252, recommandé)**
Enregistrer `http://127.0.0.1` comme redirect et autoriser n'importe quel port en
loopback. Passport ne le fait pas nativement → petit override du contrôle de redirect
(matcher `127.0.0.1`/`localhost` sur tout port). Standard pour les apps natives/desktop.

**Option B — Relais central sur mozaiklabs.fr**
Un seul redirect enregistré : `https://mozaiklabs.fr/tune/callback`. Cette page relaie
le `code` vers l'instance locale (`http://localhost:<port>/…/callback`), le port étant
transporté dans le paramètre `state`. Avantage : un seul redirect_uri à enregistrer,
propre côté Passport. Inconvénient : une page relais à écrire + confiance dans `state`.

**Option C — Device Flow (comme YouTube), zéro redirect**
mozaiklabs.fr expose un grant « device code » : Tune affiche un code + une URL
`mozaiklabs.fr/device`, l'utilisateur l'autorise dans son navigateur, Tune poll le token.
**Aucun redirect_uri**, cohérent avec YouTube. Passport ne fournit pas le device grant
par défaut → à implémenter (custom grant). C'est la voie la plus robuste pour du
self-hosted si tu es prêt à coder le grant côté Laravel.

→ **Reco : Option A** (le moins de code, standard). Option C si tu veux l'expérience la
plus propre et que coder un device grant Laravel ne te fait pas peur.

## 5. Travaux côté mozaiklabs.fr (Laravel)

- [ ] `composer require laravel/passport` + `php artisan passport:install` + migrations.
- [ ] Route/scope : décider ce que le SSO octroie (voir §8).
- [ ] Créer le **client public PKCE** « Tune » (`passport:client --public`), noter le `client_id`.
- [ ] Redirect : selon l'option choisie (A : override loopback ; B : page relais ; C : device grant).
- [ ] Endpoint `/api/user` (ou équivalent) renvoyant `{email, display_name, avatar_url, premium}` que Tune lit après login (le code Tune attend un `CloudUser`).
- [ ] CORS/HTTPS OK (déjà HTTPS).

## 6. Travaux côté Tune (Rust)

- [ ] `sso.rs` : passer en **PKCE** — générer `code_verifier` (43-128 chars), `code_challenge = base64url(sha256(verifier))`, method `S256`. Stocker le verifier le temps du flux (par `state`).
- [ ] Supprimer l'usage de `mozaik_client_secret` (client public).
- [ ] Baker le `mozaik_client_id` (const, comme YouTube) **ou** garder settings/env (déjà supporté). Reco : const + override env `TUNE_MOZAIK_CLIENT_ID`.
- [ ] `redirect_uri` : implémenter l'option retenue (A : `http://127.0.0.1:<port>/…` ; B : passer par le relais ; C : device flow → nouveau module).
- [ ] Après token : appeler `/api/user`, stocker `mozaik_access_token` + `mozaik_user`, gérer le refresh token (expiry).
- [ ] Tests unitaires (comme `authorize_url_format`), + le PKCE challenge.

## 7. Travaux côté client web

- [ ] Réactiver le bouton login quand `cloudSsoConfigured` (déjà en place via #357 — rien à défaire, ça s'activera tout seul dès que le serveur renvoie `configured=true`).
- [ ] Écran « connecté en tant que … » (déjà codé). Vérifier le rendu avatar/nom.

## 8. Ce que le SSO octroie (à décider)

- [ ] Le login mozaiklabs = simple identité, ou porte aussi le **statut premium** ?
- [ ] Un token mozaiklabs premium → Tune débloque les features premium locales ? (lien avec `license_tier`, `premium_features`).
- [ ] Scopes Passport à définir (`read-profile`, `premium`, `cloud-sync`…).
- [ ] Articulation avec le système de **licence** existant (clé de licence vs compte SSO — un seul système ou deux ?).

## 9. Plan de test

- [ ] Flux complet en local : clic login → autorisation mozaiklabs → callback → token → « connecté ».
- [ ] Port non-standard (ex. 9000) → redirect fonctionne (valide l'option A).
- [ ] Reconnexion après expiry (refresh token).
- [ ] Déconnexion (révoquer token, effacer `mozaik_*`).
- [ ] Instance sans internet → dégradation gracieuse conservée.
- [ ] 2 instances / 2 comptes → pas de collision.

## 10. TODO séquencé (ordre d'attaque)

1. **Décider** : option redirect (A/B/C) + ce que le SSO octroie (§8) + articulation licence.
2. **mozaiklabs.fr** : Passport + client public PKCE + `/api/user` + redirect selon option.
3. **Tune** : PKCE dans `sso.rs`, drop du secret, bake `client_id`, redirect.
4. **Test bout en bout** en local (port standard + non-standard).
5. **Réactiver** le bouton (automatique via `configured=true`).
6. **Rollout** progressif (garder l'opt-in, cf. règle « Tune marche 100% sans mozaiklabs.fr »).

## 11. Questions ouvertes / décisions à prendre

- [ ] Redirect : Option A (loopback), B (relais) ou C (device flow) ?
- [ ] SSO = identité seule, ou porte le premium ? Fusion avec la licence ?
- [ ] Client public PKCE confirmé (pas de secret distribué) ?
- [ ] Périmètre du premier lot : juste « se connecter + voir son profil », ou tout de suite cloud sync ?

---

**Contrainte transverse** : Tune doit continuer à marcher **100 % sans mozaiklabs.fr**
(cf. `feedback_cloud_graceful`). Le SSO reste **opt-in**, jamais bloquant.
