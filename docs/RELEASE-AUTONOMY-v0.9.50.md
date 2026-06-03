# Plan — Release Generation Autonome (cible v0.9.50)

**Statut** : Draft (2026-06-03)
**Cible** : v0.9.50 — chantier post-v0.9.0
**Effort estimé** : 8-10 jours-homme répartis sur 4 phases

---

## Vision

> Un seul trigger (push tag `vX.Y.Z` ou commande `gh workflow run release`) produit l'intégralité de la release, sans aucune intervention humaine entre le déclenchement et l'annonce publique, avec rollback automatique si une étape échoue.

---

## État actuel (2026-06-03)

### Déjà automatisé
- `release.yml` (9.8K) : build multi-plateforme (web-client, build, release jobs)
- DMG signé+notarisé macOS (Apple secrets dans repo)
- NSIS Windows (.exe installer)
- Docker multi-arch (`docker.yml`)
- Tests CI (Linux, macOS, Windows, Postgres)
- Forum watch → issues auto

### Reste manuel
- Bump versions (Cargo.toml workspace + cargo lock + Tauri config + web client package.json) — feedback `release_process` rappelle l'ordre strict
- Tag git après bump
- Vérifier que la CI est verte avant tag
- Update Homebrew formula (sha256 des tarballs)
- Push image Docker Hub avec tag versionné
- Annonce forum (post pinned)
- Cahier de recette à valider sur l'image
- Mettre à jour `~/.tune` storage releases sur le VPS

### Mémoires pertinentes
- `reference_tune_release_process` — workflow multi-plateforme complet
- `feedback_release_process` — ordre strict bump → tag
- `feedback_homebrew_cache` — users doivent untap+retap, vérifier SHA256
- `feedback_release_web_drift` — web client rebuilt depuis source en CI
- `feedback_build_deploy_discipline` — checklist post-deploy stricte

---

## Phases

### Phase 1 — Pre-flight automation (~2j)

Objectif : avant tout build, valider l'état du repo et bloquer si quelque chose cloche.

- **Workflow `preflight.yml`** déclenché sur push de tag candidat OU PR vers main avec label `release-candidate`
- Checks :
  - CI verte sur le commit
  - `cargo audit` sans CVE haute
  - `cargo deny` clean (licences + duplicates)
  - Tag follows semver et > version actuelle
  - Aucune issue forum P0 ouverte (refus si oui)
  - Tous les `TODO(release)` du code traités
  - Cahier de recette présent pour la version
- Sortie : status check bloquant sur le tag.

**Livrable** : `.github/workflows/preflight.yml` + script `scripts/preflight-check.py`

### Phase 2 — Bump + tag automation (~2j)

Objectif : un seul commande `tune-cli release bump <patch|minor|major>` met à jour TOUT.

- **Commande CLI** `tune-cli release bump` qui :
  - Lit version courante depuis `Cargo.toml` workspace
  - Calcule la nouvelle version (semver)
  - Met à jour :
    - `Cargo.toml` workspace `version = "..."` 
    - `Cargo.lock` (cargo update -w)
    - `tune-server-ipados/Tune/project.yml` (XcodeGen)
    - `web/package.json` si présent
    - `installer/installer.nsi` `!define VERSION`
    - `Formula/tune-server.rb` dans homebrew tap (séparé)
  - Commit signé `bump v0.9.50`
  - Push avec tag annoté `v0.9.50`
  - Trigger release workflow
- **Dry-run** par défaut, `--apply` pour exécuter
- Validation : aucun fichier oublié (script `scripts/find-version-strings.sh` qui grep toutes les versions et signale les divergences)

**Livrable** : nouvelle commande `tune-cli release` + `scripts/find-version-strings.sh`

### Phase 3 — Auto-changelog + auto-release-notes (~2j)

Objectif : les release notes se génèrent depuis les commits depuis la dernière tag.

- Utiliser **conventional commits** comme convention (feat:, fix:, docs:, etc.)
- Outil `git-cliff` (Rust, populaire dans la communauté Rust) configuré via `cliff.toml`
- Sections automatiques : Features, Fixes, Performance, Docs, Refactor, Breaking changes
- Mention auto des contributeurs (`git log --format='%an'` dédupliqué)
- Lien auto vers issues fermées dans le cycle
- Output : `CHANGELOG.md` mis à jour + GitHub Release body

**Livrable** : `cliff.toml` + intégration dans `release.yml`

### Phase 4 — Homebrew + Docker Hub + forum auto-publish (~3j)

Objectif : tout ce qui sort de release.yml se diffuse seul.

- **Homebrew tap auto-update** :
  - Job post-release qui édite `Formula/tune-server.rb` avec les nouveaux SHA256
  - Push direct vers `renesenses/homebrew-tap` (token séparé `HOMEBREW_TAP_TOKEN`)
  - Test du formula via `brew install --build-from-source` sur CI macOS
- **Docker Hub** :
  - Push `renesenses/tune:vX.Y.Z` + `renesenses/tune:latest` 
  - Multi-arch (amd64 + arm64)
  - Manifeste auto
- **Forum announcement** :
  - Job final qui POST `mozaiklabs.fr/api/v1/forum/threads` avec :
    - Titre : `[Release] v0.9.50 disponible`
    - Body : extrait du CHANGELOG + liens téléchargements
    - Catégorie : "Releases"
    - Pin auto (via flag dans body)
  - Token `FORUM_TOKEN` déjà configuré

**Livrable** : 3 jobs supplémentaires dans `release.yml` + secrets ajoutés

### Phase 5 — Rollback safety net (~1j)

Objectif : si quelque chose pète après la moitié des étapes, on peut annuler proprement.

- Workflow `rollback.yml` (workflow_dispatch input : `version_to_rollback`)
- Actions :
  - Mark GitHub Release as draft (re-cache)
  - Delete tag git (force) si demandé
  - Yank crates.io si appliqué
  - Revert Homebrew formula (commit previous SHA256)
  - Delete Docker Hub tag spécifique (garde `latest`)
  - Post forum errata thread
- Important : opt-in, jamais auto

**Livrable** : `.github/workflows/rollback.yml`

---

## Architecture cible

```
git tag v0.9.50
   │
   ├─► preflight.yml (status check bloquant)
   │      ├─ CI verte ?
   │      ├─ aucune P0 ouverte ?
   │      ├─ cahier de recette présent ?
   │      └─ cargo audit / deny ?
   │
   └─► release.yml (auto-déclenché si preflight OK)
          ├─ web-client (npm build)
          ├─ build (DMG, NSIS, Docker, tarballs)
          ├─ release (gh release create)
          ├─ post-release (parallèle) :
          │     ├─ Homebrew formula update
          │     ├─ Docker Hub push multi-arch
          │     ├─ Forum announcement
          │     └─ crates.io publish (oaat-* crates)
          └─ verify (smoke test installations)
```

---

## Risques et mitigations

| Risque | Mitigation |
|---|---|
| Token Homebrew exposé | Token séparé, scope minimal (repo write sur le tap uniquement) |
| Forum post raté → release sans annonce | Job retry 3x + alerte issue auto si échec final |
| Bump version partiel | Script `find-version-strings.sh` exhaustif + test CI qui grep |
| Crash après tag git mais avant publish | Phase 5 rollback workflow |
| Cassure CI sur preflight bloque les patches urgents | Bypass label `--allow-broken-ci` réservé Bertrand |
| Conventional commits pas respectés | `commitlint` en pre-commit hook + check CI |

---

## Critères de succès

- [ ] `tune-cli release bump patch` produit une release complète sans intervention en < 20 min
- [ ] 0 régression sur les releases manuelles existantes (rétro-compat workflow_dispatch)
- [ ] Test E2E : faire 3 releases successives v0.9.50-rc.1, rc.2, rc.3 sans toucher au clavier après le tag
- [ ] Rollback testé sur une release vide

---

## Ce qui n'est PAS dans ce chantier

- Auto-bump basé sur les commits (semver bump automatique selon types `feat`/`fix`) — peut être ajouté plus tard
- Notifications Slack/Discord — peut être ajouté plus tard
- A/B testing entre release channels (stable/beta) — hors scope
- Internal newsletter — hors scope

---

## Position dans la roadmap

- **Pas avant v0.9.50** : on a besoin de la roadmap v0.9.0 (Plugin SDK, Multi-user, AI, Perf 500K, Mozaiklabs OAuth, Postgres backend) d'abord
- Précède v0.9.50 → v1.0 : automatiser la release est un prérequis pour la cadence soutenue après v1.0 (releases publiques avec attentes utilisateur stables)
- Compatible avec la cadence actuelle (5-7 patches/jour) : l'auto-release fait sauter le goulot d'étranglement humain

---

## Références

- [ROADMAP-v0.9.md](ROADMAP-v0.9.md)
- [PATH-TO-v0.8.50.md](PATH-TO-v0.8.50.md)
- Memory : `reference_tune_release_process`, `feedback_release_process`, `feedback_homebrew_cache`, `feedback_release_web_drift`, `feedback_build_deploy_discipline`
- `.github/workflows/release.yml` (état actuel)

---

*Document évolutif. Dernière mise à jour : 2026-06-03.*
