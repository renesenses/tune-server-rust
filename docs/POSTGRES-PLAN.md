# Plan détaillé — Support PostgreSQL

**Axe 6 de la roadmap v0.9.0**
**Effort total estimé** : 14 jours-homme
**Priorité** : P1
**Statut** : Phase 1 + portage partiel des repos en cours sur `feature/postgres-support` (2026-06-02)

## Progrès

### Fait
- Phase 1 abstraction : `Engine` + `SqlDialect` (SQLite / Postgres impls) + `PostgresDb` skeleton (sqlx pool) + workflow CI `test-postgres.yml`
- Wiring : feature `postgres = ["dep:sqlx"]`, `SqliteDb::dialect()` / `engine()`
- **11 repos sur 14 portés** vers le `SqlDialect` (placeholders + LOWER au lieu de COLLATE NOCASE + ON CONFLICT DO NOTHING portable au lieu de INSERT OR IGNORE) :
  - settings_repo, profile_repo, rating_repo, tag_repo, radio_repo
  - source_link_repo, play_queue_repo, history_repo (partiel : full_dashboard reporté)
  - zone_repo, artist_repo, playlist_repo
- 909 tests `tune-core` verts, dont ~20 tests dialecte explicites (SQLite `?` + Postgres `$1, $2, ...`)

### Reste
- album_repo (~1054 LOC, 1 session dédiée)
- track_repo (~1124 LOC, 1 session dédiée)
- full_text_search.rs (~214 LOC, FTS5 → tsvector — phase 4 helper requis)
- Backend Postgres réellement utilisé end-to-end (PgPool dans AppState, migrations PG, repos pluggables)
- Outil CLI `tune-cli db migrate-to-postgres`
- Endpoints REST `/system/database/migrate` réellement implémentés

---

## Contexte

Tune-server-rust tourne actuellement en **SQLite uniquement** (rusqlite 0.32, bundled). La PROD .15 utilisait PostgreSQL avec l'ancien serveur Python ; la migration Rust de mai 2026 a remis SQLite par défaut.

État du code aujourd'hui :
- `sqlx = { version = "0.8", features = ["runtime-tokio", "sqlite", "postgres"] }` déjà déclarée dans `Cargo.toml` — préparation faite mais non utilisée
- `rusqlite::Connection` direct dans 17 fichiers de `tune-core/src/db/` (~8000 lignes, 14 repos)
- 55 statements `CREATE` SQLite-spécifiques dans `tune-core/src/db/migrations.rs`
- FTS5 utilisé pour la recherche full-text dans `tune-core/src/library/full_text_search.rs`
- 8 PRAGMAs SQLite-spécifiques dans `tune-core/src/db/sqlite.rs`
- Endpoints `POST /system/database/test-connection` et `POST /system/database/migrate` stub dans `tune-server/src/routes/system/database.rs:178-226` (réponse "planned for v2.1")

---

## Objectifs

1. **Choix par déploiement** : SQLite reste défaut (single-user, embedded, < 100K pistes), PostgreSQL en option (multi-user, > 500K pistes, concurrence forte)
2. **Pas de régression côté SQLite** : la majorité des utilisateurs ne touche pas à PG
3. **Migration sans perte** : outil CLI `tune db migrate-to-postgres` qui transfère row-by-row avec checksum
4. **CI dual-engine** : matrix sqlite + postgres16 sur les tests d'intégration

---

## Architecture cible

### Trait `DbBackend`

Nouveau trait dans `tune-core/src/db/backend.rs` :

```rust
#[async_trait]
pub trait DbBackend: Send + Sync {
    async fn execute(&self, sql: &str, params: &[Value]) -> Result<u64>;
    async fn query_one(&self, sql: &str, params: &[Value]) -> Result<Row>;
    async fn query_all(&self, sql: &str, params: &[Value]) -> Result<Vec<Row>>;
    async fn transaction<F, R>(&self, f: F) -> Result<R> where F: FnOnce(&mut Tx) -> Result<R>;

    fn engine(&self) -> Engine; // Sqlite | Postgres
    fn fts_match(&self, column: &str, query: &str) -> String; // diverge par engine
    fn json_extract(&self, column: &str, path: &str) -> String; // idem
}
```

Deux implémentations :
- `SqliteBackend` — wrapper autour de `rusqlite::Connection` (sync interne, async surface)
- `PostgresBackend` — wrapper autour de `sqlx::PgPool`

### Repos refactorés

Chaque repo (`TrackRepo`, `AlbumRepo`, etc.) prend `Arc<dyn DbBackend>` au lieu de `Arc<SqliteDb>`. Les requêtes SQL sont écrites dans un dialecte commun (subset compatible PG + SQLite) ou paramétrées via le trait pour les divergences (FTS, JSON).

### Migrations séparées

```
tune-core/src/db/migrations/
├── shared/        # SQL portable (la majorité)
│   ├── 001_initial.sql
│   ├── 002_add_radio.sql
│   └── ...
├── sqlite/        # specs SQLite (FTS5, PRAGMAs)
│   └── 001_fts5.sql
└── postgres/      # specs PG (tsvector + GIN, types JSONB)
    └── 001_tsvector.sql
```

Loader choisit le dossier selon le backend.

---

## Phases

### Phase 1 — Abstraction (3j)

**Livrables** :
- `tune-core/src/db/backend.rs` : trait `DbBackend` + types `Row`, `Value`, `Tx`, `Engine`
- `tune-core/src/db/sqlite_backend.rs` : impl pour rusqlite
- Tests unitaires de l'abstraction (sqlite seulement, postgres après phase 2)

**Critères** :
- Aucun changement de comportement runtime (SQLite reste défaut)
- Tous les tests existants passent
- `AppState` continue de fonctionner identiquement

---

### Phase 2 — Backend Postgres (3j)

**Livrables** :
- `tune-core/src/db/postgres_backend.rs` : impl avec `sqlx::PgPool` + `bb8`
- Config `tune.toml` : `[database] engine = "sqlite"|"postgres"`, `connection_string = "..."`
- Variable d'env `TUNE_DATABASE_URL` (override config)
- Pool config : `max_connections`, `acquire_timeout` paramétrables

**Critères** :
- Backend Postgres se connecte et exécute des requêtes basiques (smoke test)
- Health check `/system/database/status` retourne le bon engine

---

### Phase 3 — Migrations portables (2j)

**Livrables** :
- Audit des 55 CREATE statements actuels → classés en 3 catégories :
  - **Portables** (la majorité) → `migrations/shared/`
  - **SQLite-only** (FTS5, certains TRIGGER) → `migrations/sqlite/`
  - **Postgres-only** (équivalents tsvector, etc.) → `migrations/postgres/`
- Loader : `migrations::run(&backend)` choisit le bon dossier
- Schémas équivalents validés sur les deux engines

**Décisions concrètes** :
- `INTEGER PRIMARY KEY AUTOINCREMENT` (SQLite) → `BIGSERIAL PRIMARY KEY` (PG)
- `TEXT` reste portable
- `BLOB` (SQLite) → `BYTEA` (PG)
- `DATETIME` (SQLite, en TEXT ISO 8601) → `TIMESTAMPTZ` (PG)
- `JSON` stocké `TEXT` (SQLite) → `JSONB` (PG)
- Booléens : `INTEGER 0/1` (SQLite) → `BOOLEAN` (PG)

**Critères** :
- `cargo test --features postgres` initialise un schéma PG complet en moins de 5s
- `cargo test --features sqlite` reste à l'identique

---

### Phase 4 — Portage des repos (3j)

**Livrables** :
- 14 repos migrés vers `Arc<dyn DbBackend>` :
  - track_repo, album_repo, artist_repo, playlist_repo, zone_repo
  - history_repo, rating_repo, tag_repo, profile_repo, radio_repo
  - settings_repo, play_queue_repo, source_link_repo, plugin_repo
- Requêtes ré-écrites en SQL portable ou via helpers du trait
- FTS5 → `tsvector` côté PG dans `full_text_search.rs` :
  - SQLite : `WHERE fts_tracks MATCH ?`
  - Postgres : `WHERE search_tsv @@ to_tsquery('simple', ?)`
  - Refacto via `backend.fts_match(col, query)` qui retourne la clause adaptée

**Critères** :
- Suite de tests unitaires des repos passe sur sqlite ET postgres (CI matrix)
- Aucune régression de perf > 10% sur SQLite (bench `cargo bench`)

---

### Phase 5 — Outil de migration SQLite → PG (1.5j)

**Livrables** :
- Commande `tune-cli db migrate-to-postgres --source tune.db --target postgresql://...`
- Stratégie : ordre topologique des tables (artists → albums → tracks → ...)
- Idempotence : `ON CONFLICT DO NOTHING` (PG), checkpoint local
- Reporting : barre de progression par table, total tracks/albums/artists, durée
- Validation : checksums table par table avant/après (`COUNT(*)`, `SUM(hash)`)

**Critères** :
- Migration .15 prod (22884 tracks) en moins de 30 min
- Checksums identiques avant/après
- Idempotent : ré-exécution = no-op

---

### Phase 6 — Endpoint REST + UI (0.5j)

**Livrables** :
- `POST /system/database/test-connection` : ouverture réelle de connexion (au lieu du stub actuel)
- `POST /system/database/migrate` : déclenche la migration en background, retourne un task ID
- `GET /system/database/migrate/status/{task_id}` : progression
- Page web client `/settings/database` : choix engine, DSN, bouton "Tester", bouton "Migrer"

**Critères** :
- Démo complète depuis l'UI sans toucher au CLI
- Logs structurés exploitables pour debug

---

### Phase 7 — CI dual-engine (1j)

**Livrables** :
- Workflow GitHub Actions `.github/workflows/test-postgres.yml` :
  - Service `postgres:16-alpine`
  - Matrix : `engine: [sqlite, postgres]`
  - Tests d'intégration `cargo test --features ${ENGINE}` 
- Variable d'env `TEST_DATABASE_URL` injectée dans le pipeline
- Bench dual-engine optionnel (manuel)

**Critères** :
- Matrix verte sur les 3 derniers tags avant release v0.9.0
- Temps total CI < +5 min vs aujourd'hui

---

## Inventaire technique précis

### Fichiers à toucher

| Fichier | Lignes | Type de changement |
|---------|--------|--------------------|
| `tune-core/src/db/backend.rs` | nouveau | Trait DbBackend |
| `tune-core/src/db/sqlite_backend.rs` | nouveau | Impl SQLite |
| `tune-core/src/db/postgres_backend.rs` | nouveau | Impl Postgres |
| `tune-core/src/db/sqlite.rs` | ~150 | Refacto en SqliteBackend |
| `tune-core/src/db/migrations.rs` | ~900 | Découpe en shared/sqlite/postgres |
| `tune-core/src/db/track_repo.rs` | ~700 | Repo abstrait |
| `tune-core/src/db/album_repo.rs` | ~600 | Repo abstrait |
| `tune-core/src/db/artist_repo.rs` | ~400 | Repo abstrait |
| `tune-core/src/db/playlist_repo.rs` | ~500 | Repo abstrait |
| `tune-core/src/db/zone_repo.rs` | ~300 | Repo abstrait |
| `tune-core/src/db/history_repo.rs` | ~300 | Repo abstrait |
| `tune-core/src/db/rating_repo.rs` | ~200 | Repo abstrait |
| `tune-core/src/db/tag_repo.rs` | ~250 | Repo abstrait |
| `tune-core/src/db/profile_repo.rs` | ~150 | Repo abstrait |
| `tune-core/src/db/radio_repo.rs` | ~200 | Repo abstrait |
| `tune-core/src/db/settings_repo.rs` | ~100 | Repo abstrait |
| `tune-core/src/db/play_queue_repo.rs` | ~150 | Repo abstrait |
| `tune-core/src/db/source_link_repo.rs` | ~200 | Repo abstrait |
| `tune-core/src/library/full_text_search.rs` | ~400 | FTS5 / tsvector divergence |
| `tune-server/src/routes/system/database.rs` | 226 | Endpoints réellement implémentés |
| `tune-cli/src/commands/db.rs` | nouveau | Commande migrate-to-postgres |
| `tune.toml.example` | + | Section [database] |
| `.github/workflows/test-postgres.yml` | nouveau | CI matrix |
| `docs/database-postgres.md` | nouveau | Guide utilisateur |

**Total** : ~3 nouveaux fichiers, 17 fichiers modifiés.

### PRAGMAs SQLite → équivalents Postgres

| SQLite PRAGMA | Postgres équivalent |
|---------------|---------------------|
| `journal_mode=WAL` | natif (WAL toujours actif) |
| `foreign_keys=ON` | natif |
| `synchronous=NORMAL` | `synchronous_commit=on` (défaut) |
| `busy_timeout=5000` | `statement_timeout` côté connection |
| `cache_size=-64000` (64 MB) | `shared_buffers` côté serveur |
| `temp_store=MEMORY` | `temp_buffers` côté connection |
| `mmap_size=268435456` (256 MB) | N/A (PG gère via shared_buffers) |
| `analysis_limit=400` | `default_statistics_target` côté serveur |

Aucune action nécessaire côté code pour PG (config serveur).

---

## Risques détaillés

### Risque 1 — Drift de comportement entre engines

**Symptôme** : un bug n'apparaît que sur un engine (ex : ordering NULLS FIRST/LAST diffère par défaut).
**Mitigation** :
- CI matrix systématique sur 100% des tests d'intégration
- Suite de tests partagée (pas de tests SQLite-only ou PG-only sauf très spécifiques)
- Définir explicitement `NULLS FIRST/LAST` dans tous les `ORDER BY`

### Risque 2 — Perf SQLite régressée

**Symptôme** : ajouter une couche d'abstraction async ralentit les requêtes hot path.
**Mitigation** :
- `SqliteBackend` reste sync en interne, expose une surface async via `tokio::task::spawn_blocking`
- Bench dédié `cargo bench --bench library_repos` exécuté avant/après chaque phase
- Tolérance : < 10% de régression sur SQLite, gate la release

### Risque 3 — Migration trop lente sur grosses bibs

**Symptôme** : migration .15 prod (22884 tracks) prend > 1h.
**Mitigation** :
- Batch INSERT (1000 rows par batch)
- `COPY` Postgres pour les très grosses tables (tracks, history) si batch INSERT trop lent
- Mesurer dès phase 5, ajuster si besoin

### Risque 4 — Onboarding utilisateur compliqué

**Symptôme** : doc "installer PG en 3 commandes" en réalité galère pour les non-techos.
**Mitigation** :
- SQLite reste **défaut absolu**, PG est un upgrade conscient
- Image Docker `renesenses/tune` ne change pas (SQLite par défaut)
- Image alternative `renesenses/tune-pg` qui inclut postgres dans le compose
- Guide step-by-step avec captures

### Risque 5 — FTS5 → tsvector : qualité de recherche différente

**Symptôme** : tokenization différente entre FTS5 (`unicode61`) et tsvector (`simple`/`french`), résultats incohérents.
**Mitigation** :
- Tests de non-régression sur 20 requêtes types (artistes français accentués, multi-mots, etc.)
- Documenter la divergence dans le guide migration
- Si nécessaire : extension `pg_trgm` ou `pg_search` côté PG pour matcher FTS5

---

## Critères de succès (recap)

- [ ] Sur prod .15 : migration SQLite→PG réussie sans perte (22884 tracks, checksums identiques)
- [ ] Perf : P95 endpoints inchangé ±10% entre SQLite et Postgres sur bib 100K
- [ ] Doc utilisateur : guide migration en 5 étapes, < 30 min sur bib 100K
- [ ] CI matrix dual-engine 100% verte sur 3 derniers tags
- [ ] Aucune régression côté SQLite (suite de tests existante 100% verte)
- [ ] Endpoint `POST /system/database/migrate` réellement implémenté (pas de stub)
- [ ] Image Docker `renesenses/tune-pg` disponible sur Docker Hub
- [ ] Bench `cargo bench` documenté avant/après dans `docs/perf-baseline-postgres.md`

---

## Ce qui n'est PAS dans ce chantier

- **MySQL / MariaDB** : jamais demandé, hors scope
- **Sharding / réplication Postgres** : entreprise, post v1.0
- **Migration retour PG → SQLite** : outil one-way only
- **Auto-tuning Postgres** : on documente les bons réglages, on ne les pousse pas
- **Backup PG** : utiliser `pg_dump` standard, pas de wrapper Tune
- **Cluster HA** : hors scope

---

## Références

- [ROADMAP-v0.9.md](ROADMAP-v0.9.md) — axe 6
- Code actuel : `tune-core/src/db/`, `tune-server/src/routes/system/database.rs:178-226`
- Mémoire historique : `feedback_sqlite_pg_drift` (drift schéma Python avec PG)
- État baseline : `docs/perf-baseline-2026-06-02.md`

---

*Document évolutif. Dernière mise à jour : 2026-06-02.*
