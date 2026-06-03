# Path to v0.8.50 — Plan de stabilisation v0.8.x

**Statut** : Draft (2026-06-03)
**Point de départ** : v0.8.27 (main au moment de la rédaction)
**Cible** : v0.8.50 = trigger d'ouverture de `release/v0.9` et de la roadmap v0.9.0
**Stratégie** : 3 paliers (v0.8.30, v0.8.40, v0.8.50), chacun avec un thème dominant et des critères de sortie fermes

---

## Contexte

À ce stade :
- v0.8.x = série de patches post-migration Rust complète
- ~26 issues forum ouvertes (importées via le bot forum-watch le 2026-06-03)
- Multiples branches `fix/*` en parallèle (sessions agents bug-treatment)
- Roadmap v0.9.0 publiée (`docs/ROADMAP-v0.9.md`) avec 6 axes prêts à démarrer dès v0.8.50
- Axe 6 (PG abstraction) déjà mergé en avance de phase (PR #1, 2026-06-03)
- v0.8.50 = **trigger absolu** : tant qu'on n'y est pas, refus des features v0.9.0 (cf. mémoire `project_v090_trigger`)

---

## v0.8.30 — Stabilisation immédiate des bugs forum (~3 jours)

**Thème** : absorber les remontées des testeurs on-mag arrivés en mai-juin. Aucune nouvelle feature.

### In
- **Bugs P0/P1 du forum** (priorité = nombre de testeurs impactés × sévérité) :
  - #2, #6 — Création de zone cassée (multi-testeurs)
  - #15, #18 — Client web figé / blocage démarrage Windows
  - #23, #24, #26 — Plantages démarrage / sortie audio Windows
  - #14, #22 — Lecture next piste cassée
  - #11, #13 — Album multi-genres / merge d'albums (Dominique, métadonnées)
- **CI dual-engine maintenue verte** (suite PG abstraction)
- **Réponses forum systématiques** sous 48h pour chaque ticket ouvert
- **OAAT robustesse** — finaliser les fixes en cours (`fix/oaat-*`)

### Out
- Pas de nouvelle source de streaming
- Pas de nouvelle plateforme
- Pas d'optimisation perf prématurée

### Critères de sortie v0.8.30 → v0.8.31
- 0 issue forum P0 ouverte
- ≤ 3 issues forum P1 ouvertes
- Pascal, Dominique, Yves, René-Claude valident v0.8.30 sur leur setup
- CI verte sur les 3 derniers tags

---

## v0.8.40 — Performance + self-service Windows (~1 semaine)

**Thème** : préparer l'afflux post-article on-mag et baseliner la perf avant la roadmap 500K+ (axe 4 v0.9.0).

### In
- **Baseline perf élargie** :
  - Étendre `scripts/perf-baseline.sh` aux bibliothèques 100K et 200K (datasets synthétiques)
  - Cibles : library list P95 < 500ms à 100K, < 1s à 200K
  - Documenter dans `docs/perf-baseline-{date}.md` au format actuel
- **Self-service Windows** :
  - Installer NSIS qui ne demande rien (`/SILENT` par défaut OK)
  - Smoke test post-install automatique (le serveur démarre, ouvre 8888, scan dossier vide → OK)
  - Page d'accueil web → bouton "Test d'installation" qui valide DLNA + AirPlay détectés
- **Stabilité Web client** :
  - Auto-reconnect WebSocket avec backoff exponentiel
  - Indicateur "service indisponible" plutôt que figement
  - Tests E2E (Playwright) sur 3 scénarios critiques : scan, playback, zone switching
- **Documentation utilisateur** :
  - Compléter les 8 langues (les 6 non-FR/EN ont besoin de relecture native, communauté)
  - Ajouter guide troubleshooting Windows

### Out
- Pas encore d'AppState refacto pour multi-user (v0.9.0)
- Pas de DB Postgres en prod (juste abstraction prête)
- Pas de plugin SDK public (axe 1 v0.9.0)

### Critères de sortie v0.8.40 → v0.8.41
- Bench `cargo bench` documenté, P95 < cibles ci-dessus
- 3 testeurs Windows nouveaux installent sans assistance et sont opérationnels en < 15 min
- Web client 0 régression sur tests E2E
- Doc EN/FR à jour, ≥ 4 langues relues par natif (issues GitHub `doc-review-{lang}`)

---

## v0.8.50 — Gel features + ouverture release/v0.9 (~4-5 jours)

**Thème** : verrouillage. Aucune feature, uniquement consolidation.

### In
- **Audit qualité** :
  - Lancer `/ultrareview` sur main complet
  - Coverage rust : viser ≥ 60% sur `tune-core`, ≥ 50% sur `tune-server`
  - Clippy strict (`-D warnings`) sur tout le workspace
- **Migration / backup** :
  - Tester le restore d'une backup SQLite v0.7 → v0.8.50 sur dataset réel (.15 prod en clone)
  - Documenter le rollback v0.8.50 → v0.8.49 pas-à-pas
- **Branche `release/v0.9`** :
  - Créer `release/v0.9` depuis main à v0.8.50
  - `main` continue d'accepter des fixes (les hotfixes back-portent vers `release/v0.9`)
  - Branche `feature/*` pour chaque axe v0.9.0 (plugin-sdk, multi-user, ai-assistant, perf-500k, mozaiklabs-oauth, postgres-backend)
- **Annonce forum** :
  - Post pinned annonçant le passage à v0.9
  - Liste des 6 axes, priorité, ordre prévu
  - Appel à relecture roadmap (Pascal, Dominique avancés)

### Out
- Aucune nouvelle feature (gel strict)

### Critères de sortie v0.8.50 → branche release/v0.9 ouverte
- 0 issue forum P0/P1 ouverte
- ≥ 90% des tests passants (suite complète : tune-core + tune-server + tune-cli + intégration)
- Aucune régression sur les bench perf vs v0.8.40
- Image Docker `renesenses/tune:v0.8.50` poussée sur Docker Hub avec multi-arch
- Tag GitHub `v0.8.50` avec release notes complètes

---

## Quoi NE PAS faire entre v0.8.27 et v0.8.50

Pour rappeler le principe : tout ce qui est sur la roadmap v0.9.0 est **refusé/différé**, y compris :

- **Plugin SDK public** (axe 1 v0.9.0)
- **Multi-utilisateur + auth** (axe 2 v0.9.0)
- **AI assistant intégré** (axe 3 v0.9.0)
- **Mozaiklabs.fr OAuth + share now playing** (axe 5 v0.9.0)
- **Wiring backend PG end-to-end** (axe 6, abstraction déjà mergée mais pas le PgPool dans AppState)

Si un testeur demande ces features : « C'est sur la roadmap v0.9 (Q3 2026), pas avant. ».

Si une opportunité commerciale (article presse, démo) demande une de ces features : faire remonter à Bertrand avant tout démarrage, le trigger v0.8.50 reste la règle par défaut.

---

## Cadence et rituels

- **Patch releases plusieurs fois par jour** au rythme des fixes (cadence actuelle 5-7 patches/jour)
- **Bilan à chaque palier dizaine** (v0.8.30, v0.8.40, v0.8.50) — point sur les critères de sortie restants
- **Réponse forum** : 24h SLA dès maintenant (la cadence quotidienne le permet déjà)
- **Test plan** (cahier de recette) re-validé à chaque palier majeur (v0.8.30, v0.8.40, v0.8.50)

---

## Versionnage / nommage

| Palier | Cible date | Nom interne |
|--------|------------|-------------|
| v0.8.30 | 2026-06-06 | "Stabilisation forum" |
| v0.8.40 | 2026-06-13 | "Self-service Windows + perf baseline" |
| v0.8.50 | 2026-06-18 | "Gel + ouverture v0.9" |

Cadence calibrée sur le rythme observé : 5-7 patches/jour (cf. sessions mai-juin :
v0.7.96→v0.7.100 le 17 mai, v0.8.14→v0.8.19 le 1er juin, v0.8.22→v0.8.27
sur le week-end 2-3 juin). Dates ajustables selon flux de bugs forum.

---

## Références

- [ROADMAP-v0.9.md](ROADMAP-v0.9.md) — la cible après v0.8.50
- [POSTGRES-PLAN.md](POSTGRES-PLAN.md) — axe 6 partiellement livré (abstraction)
- `docs/cahier-recette-v0.8.20.md` — test plan à re-valider à chaque palier
- `docs/perf-baseline-2026-06-02.md` — point de départ perf (22884 tracks .15, P95 ≤ 3ms)
- Memory `project_v090_trigger` — règle de gel v0.8.50

---

*Document évolutif. Dernière mise à jour : 2026-06-03.*
