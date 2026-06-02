# Roadmap v0.9.0

Document de cadrage pour la prochaine release majeure de Tune. Cinq axes prioritaires définis post-v0.8.22.

**Cible** : Q3 2026
**Statut** : Draft (2026-06-02)
**Audience** : équipe + testeurs avancés (relecture Pascal, Dominique attendue)

---

## Vue d'ensemble

| # | Axe | Effort | Priorité | Statut deps |
|---|-----|--------|----------|-------------|
| 1 | Plugin SDK public | 15j | P0 | Archi 70% prête (v0.8) |
| 2 | Multi-utilisateur + auth | 10j | P1 | Schéma DB à ajouter |
| 3 | AI assistant intégré | 8j | P2 | Endpoint Anthropic/OpenAI |
| 4 | Performance 500K+ pistes | 12j | P1 | Refacto FTS5 + scan |
| 5 | Intégration Mozaiklabs.fr | 6j | P2 | OAuth côté site PHP |

**Total estimé** : ~51 jours-homme. Cible release : 2026-09 si on enchaîne sans accroc.

---

## 1. Plugin SDK public

### Scope
**In** :
- Crate `tune-plugin-sdk` publiée sur crates.io
- Trait `TunePlugin` + types `PluginContext`, `PluginCapabilities`, `PluginEvent`
- Loader dynamique via `libloading` (cdylib) OU via Python `entry_points` (la RFC interne couvre les deux)
- 6 PoC plugins : recorder, last.fm scrobbler, ListenBrainz scrobbler, visualizer (websocket), discord rich presence, custom DSP filter
- Documentation développeur sur `docs/plugins/` (FR + EN)
- Exemples buildables avec `cargo build` standalone

**Out** :
- Hot-reload (restart serveur requis pour charger un nouveau plugin)
- Sandbox/permissions fines (les plugins ont accès complet au PluginContext)
- Marketplace plugins (Q4, axe v1.0)

### Dépendances techniques
- Stabiliser les types publics de `tune-core` (interner et freezer un sous-ensemble)
- Définir une politique de SemVer pour les breaking changes du SDK
- CI : build matrix Linux/macOS/Windows pour les exemples

### Risques
- ABI Rust instable : entre versions de rustc, un cdylib peut planter. Mitigation : compiler tous les plugins officiels dans le même CI que le serveur, ne distribuer que des binaires alignés.
- Tentation de tout exposer dans le SDK → bloat. Mitigation : commencer par 8 points d'extension précis (player events, scan events, custom routes, etc.)

### Critères de succès
- Un dev externe peut écrire un plugin scrobbler en < 2h avec uniquement la doc
- Recorder migré comme plugin externe (binaire séparé)
- Au moins 1 plugin tiers communautaire sur le forum à la release

### Référence
Voir `project_tune_plugin_rfc` en mémoire pour le détail technique.

---

## 2. Multi-utilisateur + auth

### Scope
**In** :
- Table `users` : id, email, password_hash (argon2), role, created_at
- Sessions JWT (cookie httponly + bearer pour API)
- RBAC simple : `admin` (tout), `user` (lecture + playback), `guest` (lecture seule)
- Middleware Axum `require_auth(min_role)` sur chaque route
- UI Settings → Users (admin) : créer/supprimer/changer rôle
- UI login page (avant tout accès)
- Migration : utilisateur seul existant promu admin, mot de passe défini au premier login

**Out** :
- SSO/OAuth tiers (Google/GitHub) → reporté v1.0
- 2FA → reporté v1.0
- Audit log détaillé → reporté

### Dépendances techniques
- Choisir lib JWT (`jsonwebtoken` crate)
- Refactor : passer `Option<User>` dans tous les handlers (extractor Axum)
- Web client : intercepteur 401 → redirect /login
- iPad client : ajouter écran login + stockage token Keychain

### Risques
- Casser les setups existants single-user → mitigation : mode "no-auth" gardé en option `TUNE_AUTH=disabled` (warning au boot)
- Impact perf JWT verify sur hot paths (now-playing polling) → mitigation : cache LRU des verifs

### Critères de succès
- Famille/colocataires peuvent partager une instance Tune avec rôles distincts
- Pas de régression perf sur endpoints chauds (< 5% latency added)
- Migration silencieuse pour les setups single-user existants

---

## 3. AI assistant intégré

### Scope
**In** :
- Endpoint `POST /api/v1/ai/query` : input texte naturel, output action structurée + commentaire texte
- Intégration backend : appel Anthropic Claude (Sonnet) ou OpenAI au choix dans Settings
- Système de tools : le LLM peut appeler `play_album(id)`, `search_tracks(query)`, `add_to_queue(ids)`, `set_zone(id)`, `pause()`, etc.
- UI : bouton micro/clavier dans la barre du haut, modal de conversation
- Mémoire courte : 5 derniers messages par utilisateur (pas de persistance long terme)
- Quelques exemples publics : "joue du jazz cool", "passe sur la zone salon", "ajoute le dernier album de Radiohead à la file", "qu'est-ce qui joue ?"

**Out** :
- Wake word / voix toujours active → out
- Recommandations proactives → out
- Local LLM (Ollama) → reporté v1.0 (mais architecture pensée pour)

### Dépendances techniques
- Clé API Anthropic/OpenAI : choisie par l'utilisateur, stockée chiffrée
- Quotas : limite à N requêtes/jour par utilisateur (configurable)
- Privacy : préciser dans la doc que la requête + métadonnées (titre courant, zone active) sont envoyées au LLM choisi

### Risques
- Hallucinations LLM → mitigation : valider chaque action avant exécution, demander confirmation pour actions destructives
- Coût utilisateur imprévisible → mitigation : afficher coût estimé + quota
- Latence (2-5s) → mitigation : indicateur "réfléchit..." dans l'UI

### Critères de succès
- 10 commandes naturelles fonctionnent de bout en bout
- < 5% de réponses cassent l'UX (timeout/erreur API)
- Démontrable en 1 minute lors d'une présentation

---

## 4. Performance 500K+ pistes

### Scope
**In** :
- Profiler les requêtes Library sur dataset synthétique 500K (générer via script)
- Indexes SQLite avancés : composites album+artist, dates, format
- FTS5 améliorations : tokeniser sur artists, splits MBID, support quotes
- Lazy loading albums dans web client (virtual scroll React déjà partiel)
- Scan parallèle multi-thread (actuellement séquentiel par dossier)
- Cache LRU artwork au niveau serveur (actuellement re-lu disque à chaque requête)
- Bench script reproducible : `cargo bench --bench library_500k`

**Out** :
- Migration vers PostgreSQL → reporté (gros chantier)
- Sharding → out
- Distribution multi-node → out

### Dépendances techniques
- Augmenter `cache_size` SQLite par défaut
- Évaluer `mmap_size` selon RAM dispo
- Refacto code de scan pour parallélisme safe (tokio JoinSet bounded)

### Risques
- Régression sur petites bibliothèques (overhead caches inutiles) → mitigation : adapter selon taille bib détectée au démarrage
- SQLite locking sur écritures concurrentes → mitigation : WAL mode + write queue

### Critères de succès
- Bibliothèque 500K : scan complet < 2h
- Library list (page 1) : P95 < 200ms
- Recherche FTS5 : P95 < 500ms sur 500K tracks
- RSS serveur < 200MB en idle après scan

---

## 5. Intégration Mozaiklabs.fr

### Scope
**In** :
- OAuth login Mozaiklabs : bouton "Connecter avec Mozaiklabs" sur Tune login
- Compte unique : forum + site + Tune partagent l'identité
- "Share now playing" : depuis Tune, publier un blog post sur Mozaiklabs (track + commentaire libre)
- Téléchargements officiels via mozaiklabs.fr/downloads (mirroir GitHub Releases)
- Page profil utilisateur sur site : afficher derniers tracks joués via API Tune (opt-in)

**Out** :
- Sync playlists collaboratives via Mozaiklabs → reporté v1.0
- Système de likes / commentaires sur tracks partagés → reporté
- Activity feed temps réel → reporté

### Dépendances techniques
- Côté site PHP : implémenter serveur OAuth2 (Laravel Passport ou custom)
- Côté Tune : client OAuth2 (`oauth2` crate Rust)
- API site Mozaiklabs : `POST /api/v1/posts` (créer post), `GET /api/v1/me` (info profil)
- HTTPS obligatoire pour la callback URL Tune (gestion mode dev/prod)

### Risques
- Bouton OAuth si l'instance Tune n'est pas joignable depuis Internet → mitigation : flow alternatif token manuel
- Dépendance site Mozaiklabs en down → mitigation : graceful fallback login local

### Critères de succès
- Un utilisateur peut s'inscrire sur Mozaiklabs et utiliser le même compte sur Tune
- "Share now playing" publie en < 3s
- mozaiklabs.fr/downloads sert les derniers releases avec moins de 5 min de latence vs GitHub

---

## Matrice priorité × effort

```
Priorité ↑
   P0  | 1. Plugin SDK
       |
   P1  | 4. Perf 500K        2. Multi-user
       |
   P2  | 5. Mozaiklabs       3. AI assistant
       |
       +-----------------------------------> Effort
        Faible        Moyen        Lourd
```

**Ordre d'exécution suggéré** :
1. **Plugin SDK** d'abord (débloque la communauté, archi prête)
2. **Perf 500K** en parallèle (pas de dépendance entre les deux, équipe split)
3. **Multi-user** ensuite (prérequis pour Mozaiklabs OAuth)
4. **Mozaiklabs intégration** (post multi-user)
5. **AI assistant** en dernier (le plus exploratoire, peut être différé à v0.9.1)

---

## Hors scope v0.9.0

Repoussé à v1.0 ou ultérieur :
- Mobile native iOS/Android (autre que iPad existant)
- Sync playlists cross-services (Soundiiz-like) — voir [v0.7.30-39 roadmap](../README.md)
- LEEDH volume control intégré (en attente réponse de Gilles Milot, mémo `project_leedh_processing`)
- Apple Watch app — post v1.0
- HomePod direct (sans AirPlay routing)
- Mode offline iPad complet

---

## Process

- **Relecture** : Pascal (bluevelvet, audiophile Windows), Dominique (Windows, métadonnées), Yves (DMG/DLNA)
- **Issues GitHub** : chaque axe créé comme `Epic` avec checklist
- **Communication forum** : annonce roadmap dès validation (post Mozaiklabs `[Roadmap] v0.9.0`)
- **Cadence** : milestones intermédiaires v0.8.30, v0.8.40, v0.8.50 avant la bascule v0.9.0

---

*Document évolutif. Dernière mise à jour : 2026-06-02.*
