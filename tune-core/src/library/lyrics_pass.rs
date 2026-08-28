//! Paroles : indicateur de couverture + passes de fond (issue #2172).
//!
//! Jusqu'ici les paroles n'étaient récupérées **qu'à la demande**, au moment où
//! une piste est jouée (`crate::lyrics::get_lyrics`, routes
//! `/library/tracks/{id}/lyrics` et `/lyrics/by-meta`). Rien ne les rassemblait
//! pour la bibliothèque, et **rien ne savait lesquelles en ont**.
//!
//! Ce module apporte les deux moitiés, dans cet ordre de risque croissant :
//!
//! 1. **L'indicateur** ([`coverage`]) — du SQL, rien d'autre. Aucun réseau,
//!    aucune entrée/sortie fichier, aucune migration : les trois sources que la
//!    cascade d'affichage consulte sont déjà toutes en base.
//! 2. **La passe locale** ([`run_local_index`]) — repère les fichiers `.lrc`
//!    voisins et les inscrit dans `track_metadata`. Système de fichiers
//!    seulement, aucun tiers.
//! 3. **La passe LRCLIB** ([`run_lrclib_fill`]) — la seule qui sorte sur le
//!    réseau, et donc la seule sous conditions strictes (voir plus bas).
//!
//! # Les trois sources, et où elles vivent déjà
//!
//! | source   | où c'est en base                              | qui l'écrit |
//! |----------|-----------------------------------------------|-------------|
//! | `lrc`    | `track_metadata['lyrics_lrc']` (chemin)       | [`run_local_index`] |
//! | `tag`    | `track_metadata['lyrics']`                    | le scan (`read_extended_metadata`) |
//! | `lrclib` | `lyrics_cache` (ligne non vide)               | la route à la demande, et [`run_lrclib_fill`] |
//!
//! # Ce que la passe LRCLIB doit à un service tiers gratuit
//!
//! - **Consentement.** [`run_lrclib_fill`] relit le réglage
//!   `lyrics_lrclib_enabled` — la **même** clé que la récupération à la demande
//!   — et rend [`FillStatus::Refused`] sans émettre la moindre requête tant
//!   qu'il ne vaut pas `"true"`. La garde est ici, dans le cœur, pas seulement
//!   dans la route.
//! - **Débit.** [`FillOptions::limiter`] — en production le limiteur partagé
//!   [`crate::http::fetch::LRCLIB`], ~1 req/s — est acquis **avant chaque**
//!   requête, et un run est plafonné à [`FillOptions::max_requests`].
//! - **Obéissance.** Au premier 429/503 ([`FetchFailure::RateLimited`], dérivé
//!   du compteur [`crate::lyrics::LRCLIB_RATE_LIMIT_HITS`]) la passe
//!   **s'arrête**. Elle est reprenable : ne rien forcer ne coûte rien.
//!
//! # Ne pas repayer ce qui est déjà payé
//!
//! `lyrics_cache` garde **la trace de la tentative, pas seulement du succès** :
//! une recherche infructueuse y écrit une ligne aux deux corps vides
//! (`store_cache_entry` avec `None`/`None`). Deux gardes s'appuient dessus :
//! la sélection des candidates ([`candidates`], en SQL) et la vérification
//! juste avant l'appel ([`crate::lyrics::LyricsCacheEntry::spares_a_fetch`], en
//! Rust) — la même règle exactement que la route à la demande. Une piste sans
//! paroles n'est donc réinterrogée qu'après
//! [`crate::lyrics::NEGATIVE_CACHE_TTL_DAYS`] jours, pas à chaque passe.
//!
//! C'est aussi ce qui rend la passe **reprenable** : une passe interrompue a
//! déjà écrit une ligne pour chaque piste traitée, donc la suivante reprend
//! exactement où l'autre s'est arrêtée, sans registre d'avancement séparé.

use std::sync::Arc;
use std::sync::atomic::Ordering;

use serde::Serialize;
use tracing::{debug, info, warn};

use crate::db::backend::{DbBackend, ToSqlValue};
use crate::db::engine::{Engine, PostgresDialect, SqlDialect, SqliteDialect};
use crate::http::fetch::RateLimiter;
use crate::lyrics::{LRCLIB_RATE_LIMIT_HITS, LrclibRaw};

/// Clé `track_metadata` posée par [`run_local_index`] : le chemin du `.lrc`
/// voisin trouvé. Distincte de `lyrics` (étiquette embarquée, posée au scan)
/// pour que l'indicateur puisse nommer la source.
pub const META_KEY_SIDECAR: &str = "lyrics_lrc";

/// Clé `track_metadata` que le scan remplit depuis l'étiquette du fichier
/// (USLT / LYRICS) — voir `metadata::read_extended_metadata`.
pub const META_KEY_TAG: &str = "lyrics";

/// Réglage de consentement pour LRCLIB. **Même clé** que la récupération à la
/// demande (`routes/library/tracks.rs`, `routes/lyrics.rs`) : un utilisateur
/// qui n'a pas autorisé l'appel réseau piste par piste ne l'a pas davantage
/// autorisé pour toute sa bibliothèque.
pub const SETTING_LRCLIB_ENABLED: &str = "lyrics_lrclib_enabled";

/// Réglage où la passe publie son avancement puis son bilan (JSON).
pub const SETTING_FILL_RESULT: &str = "lyrics_fetch_result";

/// Nombre d'échecs réseau **consécutifs** au-delà duquel la passe abandonne :
/// au troisième, ce n'est plus une piste introuvable, c'est le lien qui est
/// coupé (ou le service qui refuse), et insister ne sert personne.
const MAX_CONSECUTIVE_FAILURES: usize = 3;

/// Clé du limiteur de débit : un seul créneau pour tout LRCLIB.
const LIMITER_KEY: &str = "lrclib";

// ---------------------------------------------------------------------------
// Prédicats SQL partagés
// ---------------------------------------------------------------------------
//
// Neutres vis-à-vis du moteur (SQLite et PostgreSQL) : `EXISTS`, `<>` et les
// comparaisons de texte se comportent pareil. Aucun paramètre lié ici, donc ils
// s'assemblent librement dans les requêtes ci-dessous.

/// La piste a des paroles depuis un `.lrc` voisin.
///
/// Deux emplacements, parce que deux chemins de code les écrivent :
/// `tracks.synced_lyrics` (posé opportunément par la route
/// `/tracks/{id}/synced-lyrics` quand elle lit un sidecar) et la clé
/// `track_metadata['lyrics_lrc']` (posée par [`run_local_index`]).
const HAS_LRC: &str = "((t.synced_lyrics IS NOT NULL AND t.synced_lyrics <> '') \
     OR EXISTS (SELECT 1 FROM track_metadata m WHERE m.track_id = t.id \
                AND m.key = 'lyrics_lrc' AND m.value <> ''))";

/// La piste a des paroles dans son étiquette embarquée (posées au scan).
const HAS_TAG: &str = "EXISTS (SELECT 1 FROM track_metadata m WHERE m.track_id = t.id \
     AND m.key = 'lyrics' AND m.value <> '')";

/// `lyrics_cache` porte des paroles pour cette piste (positif : n'expire pas).
const HAS_LRCLIB: &str = "EXISTS (SELECT 1 FROM lyrics_cache c WHERE c.track_id = t.id \
     AND ((c.synced_lyrics IS NOT NULL AND c.synced_lyrics <> '') \
       OR (c.plain_lyrics IS NOT NULL AND c.plain_lyrics <> '')))";

/// LRCLIB a déjà été interrogé pour cette piste et n'avait rien — la trace de
/// tentative, celle qui évite de repayer.
const SEARCHED_EMPTY: &str = "EXISTS (SELECT 1 FROM lyrics_cache c WHERE c.track_id = t.id \
     AND (c.synced_lyrics IS NULL OR c.synced_lyrics = '') \
     AND (c.plain_lyrics IS NULL OR c.plain_lyrics = ''))";

fn dialect_sql(db: &Arc<dyn DbBackend>, f: impl Fn(&dyn SqlDialect) -> String) -> String {
    match db.engine() {
        Engine::Sqlite => f(&SqliteDialect),
        Engine::Postgres => f(&PostgresDialect),
    }
}

// ---------------------------------------------------------------------------
// L'indicateur
// ---------------------------------------------------------------------------

/// Ce que la bibliothèque sait de ses paroles. Répond à « rien ne sait ce qui
/// en a » : combien en ont, d'où elles viennent, et — pour celles qui n'en ont
/// pas — si on a déjà cherché.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub struct LyricsCoverage {
    pub total_tracks: i64,
    /// Paroles depuis un `.lrc` voisin.
    pub from_lrc: i64,
    /// Paroles depuis l'étiquette embarquée (aucune `.lrc` voisine).
    pub from_tag: i64,
    /// Paroles depuis LRCLIB (ni `.lrc`, ni étiquette).
    pub from_lrclib: i64,
    /// Total des trois ci-dessus.
    pub with_lyrics: i64,
    /// `total_tracks - with_lyrics`.
    pub without_lyrics: i64,
    /// Parmi `without_lyrics` : LRCLIB a déjà été interrogé et n'avait rien.
    /// Ces pistes ne seront pas réinterrogées avant l'expiration du négatif.
    pub searched_no_result: i64,
    /// Parmi `without_lyrics` : jamais cherchées. C'est le travail qui reste.
    pub never_searched: i64,
}

/// Compte la couverture en **une** requête, sans réseau ni accès disque.
///
/// Les trois sources sont exclusives dans ce décompte, et dans l'ordre exact de
/// la cascade d'affichage (`.lrc` > étiquette > LRCLIB) : une piste qui a un
/// `.lrc` *et* une entrée LRCLIB est comptée en `from_lrc`, parce que c'est le
/// `.lrc` que l'utilisateur verra. Les trois colonnes s'additionnent donc
/// exactement en `with_lyrics`.
pub fn coverage(db: &Arc<dyn DbBackend>) -> Result<LyricsCoverage, String> {
    let sql = format!(
        "SELECT \
         (SELECT COUNT(*) FROM tracks), \
         (SELECT COUNT(*) FROM tracks t WHERE {HAS_LRC}), \
         (SELECT COUNT(*) FROM tracks t WHERE NOT {HAS_LRC} AND {HAS_TAG}), \
         (SELECT COUNT(*) FROM tracks t WHERE NOT {HAS_LRC} AND NOT {HAS_TAG} AND {HAS_LRCLIB}), \
         (SELECT COUNT(*) FROM tracks t WHERE NOT {HAS_LRC} AND NOT {HAS_TAG} \
            AND NOT {HAS_LRCLIB} AND {SEARCHED_EMPTY})"
    );

    let row = db.query_one(&sql, &[])?.unwrap_or_default();
    let get = |i: usize| row.get(i).and_then(|v| v.as_i64()).unwrap_or(0);

    let total_tracks = get(0);
    let from_lrc = get(1);
    let from_tag = get(2);
    let from_lrclib = get(3);
    let searched_no_result = get(4);

    let with_lyrics = from_lrc + from_tag + from_lrclib;
    let without_lyrics = (total_tracks - with_lyrics).max(0);

    Ok(LyricsCoverage {
        total_tracks,
        from_lrc,
        from_tag,
        from_lrclib,
        with_lyrics,
        without_lyrics,
        searched_no_result,
        never_searched: (without_lyrics - searched_no_result).max(0),
    })
}

// ---------------------------------------------------------------------------
// Sélection des candidates
// ---------------------------------------------------------------------------

/// Une piste que la passe LRCLIB pourrait interroger.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LyricsCandidate {
    pub track_id: i64,
    pub title: String,
    pub artist: String,
    pub album: Option<String>,
    pub duration_ms: i64,
}

/// Pistes sans paroles connues **et** qui ne coûteraient pas une requête déjà
/// payée : ni `.lrc`, ni étiquette, ni entrée `lyrics_cache` encore valable
/// (positive, ou négative de moins de
/// [`crate::lyrics::NEGATIVE_CACHE_TTL_DAYS`] jours).
///
/// Les pistes sans artiste ou sans titre sont écartées : LRCLIB exige les deux,
/// la route à la demande s'arrête pareil (`if artist.is_empty()`), et les
/// interroger serait une requête garantie perdue.
///
/// `after_id` permet de dérouler la bibliothèque par tranches ; l'ordre est
/// l'ordre des identifiants, stable d'un run à l'autre.
pub fn candidates(
    db: &Arc<dyn DbBackend>,
    after_id: i64,
    limit: i64,
) -> Result<Vec<LyricsCandidate>, String> {
    let cutoff = crate::lyrics::negative_retry_cutoff();
    let sql = dialect_sql(db, |d| {
        format!(
            "SELECT t.id, t.title, ar.name, al.title, t.duration_ms \
             FROM tracks t \
             JOIN artists ar ON ar.id = t.artist_id \
             LEFT JOIN albums al ON al.id = t.album_id \
             WHERE t.id > {p1} \
               AND t.title <> '' AND ar.name <> '' \
               AND NOT {HAS_LRC} AND NOT {HAS_TAG} AND NOT {HAS_LRCLIB} \
               AND NOT EXISTS (SELECT 1 FROM lyrics_cache c \
                               WHERE c.track_id = t.id AND c.fetched_at >= {p2}) \
             ORDER BY t.id LIMIT {p3}",
            p1 = d.placeholder(1),
            p2 = d.placeholder(2),
            p3 = d.placeholder(3),
        )
    });

    let params: [&dyn ToSqlValue; 3] = [&after_id, &cutoff, &limit];
    Ok(db
        .query_many(&sql, &params)?
        .into_iter()
        .filter_map(|r| {
            Some(LyricsCandidate {
                track_id: r.first()?.as_i64()?,
                title: r.get(1)?.as_string()?,
                artist: r.get(2)?.as_string()?,
                album: r.get(3).and_then(|v| v.as_string()),
                duration_ms: r.get(4).and_then(|v| v.as_i64()).unwrap_or(0),
            })
        })
        .collect())
}

// ---------------------------------------------------------------------------
// Passe 1 — locale (fichiers `.lrc` voisins). Aucun réseau.
// ---------------------------------------------------------------------------

/// Bilan de la passe locale.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub struct LocalIndexReport {
    /// Pistes regardées (celles qui ont un chemin de fichier et pas encore de
    /// `.lrc` connu).
    pub examined: usize,
    /// Pistes pour lesquelles un `.lrc` voisin non vide a été inscrit.
    pub found: usize,
}

/// Pistes à examiner par la passe locale : un chemin de fichier, et pas encore
/// de `.lrc` connu. Les paroles LRCLIB ne disqualifient pas : un `.lrc` posé
/// par l'utilisateur prime sur ce qu'on avait téléchargé, et l'indicateur doit
/// le dire.
fn local_index_candidates(
    db: &Arc<dyn DbBackend>,
    after_id: i64,
    limit: i64,
) -> Result<Vec<(i64, String)>, String> {
    let sql = dialect_sql(db, |d| {
        format!(
            "SELECT t.id, t.file_path FROM tracks t \
             WHERE t.id > {p1} AND t.file_path IS NOT NULL AND t.file_path <> '' \
               AND NOT {HAS_LRC} \
             ORDER BY t.id LIMIT {p2}",
            p1 = d.placeholder(1),
            p2 = d.placeholder(2),
        )
    });
    let params: [&dyn ToSqlValue; 2] = [&after_id, &limit];
    Ok(db
        .query_many(&sql, &params)?
        .into_iter()
        .filter_map(|r| {
            let id = r.first()?.as_i64()?;
            let path = r.get(1)?.as_string()?;
            Some((id, path))
        })
        .collect())
}

/// Repère les `.lrc` voisins de toute la bibliothèque et les inscrit dans
/// `track_metadata`, pour que l'indicateur soit **honnête** : sans cette passe,
/// un utilisateur qui range ses paroles en fichiers `.lrc` serait compté comme
/// n'en ayant aucune.
///
/// Bloquant (accès disque) : à appeler sous `spawn_blocking`. Aucun réseau,
/// aucun consentement à demander, **aucune écriture dans les dossiers de
/// musique** — on ne fait que lire et noter le chemin trouvé.
///
/// `progress(traitées, total)` est appelé au fil de l'eau.
pub fn run_local_index(
    db: &Arc<dyn DbBackend>,
    total_hint: usize,
    mut progress: impl FnMut(usize, usize),
) -> LocalIndexReport {
    const BATCH: i64 = 500;
    let meta = crate::db::track_metadata_repo::TrackMetadataRepo::with_backend(db.clone());
    let mut report = LocalIndexReport::default();
    let mut after_id = 0i64;

    loop {
        let batch = match local_index_candidates(db, after_id, BATCH) {
            Ok(b) => b,
            Err(e) => {
                warn!(error = %e, "lyrics_local_index_list_failed");
                break;
            }
        };
        if batch.is_empty() {
            break;
        }
        for (track_id, path) in batch {
            after_id = track_id;
            report.examined += 1;
            let Some(sidecar) = crate::metadata::lyrics::sidecar_lrc_path(&path) else {
                continue;
            };
            // Un `.lrc` vide n'est pas des paroles : on ne veut pas d'un
            // indicateur qui annonce des paroles et n'affiche rien.
            let has_body = std::fs::read_to_string(&sidecar)
                .map(|c| !c.trim().is_empty())
                .unwrap_or(false);
            if !has_body {
                continue;
            }
            let value = sidecar.to_string_lossy().to_string();
            if let Err(e) = meta.set(track_id, META_KEY_SIDECAR, &value) {
                warn!(track_id, error = %e, "lyrics_local_index_store_failed");
                continue;
            }
            report.found += 1;
            debug!(track_id, sidecar = %value, "lyrics_local_index_found");
        }
        progress(report.examined, total_hint.max(report.examined));
    }

    info!(
        examined = report.examined,
        found = report.found,
        "lyrics_local_index_done"
    );
    report
}

// ---------------------------------------------------------------------------
// Passe 2 — LRCLIB. Réseau, donc sous conditions.
// ---------------------------------------------------------------------------

/// Pourquoi une requête LRCLIB a échoué. Le distinguo est ce qui décide de la
/// suite : un « ralentis » arrête la passe net, une panne de transport ne
/// l'arrête qu'après [`MAX_CONSECUTIVE_FAILURES`] d'affilée.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FetchFailure {
    /// HTTP 429 / 503 — le service demande explicitement qu'on s'arrête.
    RateLimited,
    /// Panne réseau ou protocole.
    Transport(String),
}

/// Adapte [`crate::lyrics::fetch_lrclib_raw`] au contrat attendu par
/// [`run_lrclib_fill`] : une erreur y devient [`FetchFailure::RateLimited`]
/// quand — et seulement quand — **cet appel-ci** a fait progresser
/// [`crate::lyrics::LRCLIB_RATE_LIMIT_HITS`], le compteur que `fetch_lrclib_raw`
/// incrémente sur 429/503.
///
/// L'instantané pris autour du seul appel évite de confondre notre 429 avec
/// celui d'un autre appelant.
pub async fn fetch_for_pass(
    client: &reqwest::Client,
    cand: &LyricsCandidate,
) -> Result<Option<LrclibRaw>, FetchFailure> {
    let before = LRCLIB_RATE_LIMIT_HITS.load(Ordering::Relaxed);
    let duration_secs = (cand.duration_ms > 0).then_some(cand.duration_ms / 1000);
    let res = crate::lyrics::fetch_lrclib_raw(
        client,
        &cand.artist,
        &cand.title,
        cand.album.as_deref(),
        duration_secs,
    )
    .await;
    match res {
        Ok(raw) => Ok(raw),
        Err(e) => {
            if LRCLIB_RATE_LIMIT_HITS.load(Ordering::Relaxed) > before {
                Err(FetchFailure::RateLimited)
            } else {
                Err(FetchFailure::Transport(e))
            }
        }
    }
}

/// Comment un run s'est terminé.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum FillStatus {
    /// Plus aucune candidate : la bibliothèque est à jour.
    Done,
    /// `lyrics_lrclib_enabled` ne vaut pas `"true"`. **Aucune requête émise.**
    Refused,
    /// Plafond [`FillOptions::max_requests`] atteint : il reste du travail,
    /// relancer reprendra là où on s'est arrêté.
    Capped,
    /// LRCLIB a répondu 429/503 : on s'arrête net.
    RateLimited,
    /// Trop d'échecs réseau consécutifs.
    NetworkError,
}

/// Bilan d'un run de la passe LRCLIB.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct FillReport {
    pub status: FillStatus,
    /// Candidates parcourues.
    pub examined: usize,
    /// Requêtes réellement émises vers LRCLIB.
    pub requested: usize,
    /// Requêtes ayant rapporté des paroles.
    pub found: usize,
    /// Requêtes ayant répondu « rien » — mises en cache, donc non repayées.
    pub not_found: usize,
    /// Requêtes en échec réseau/protocole — rien mis en cache, retentées.
    pub failed: usize,
    /// Candidates écartées juste avant l'appel parce qu'une entrée de cache
    /// encore valable les dispensait (course avec la route à la demande).
    pub skipped_already_paid: usize,
}

impl Default for FillReport {
    fn default() -> Self {
        Self {
            status: FillStatus::Done,
            examined: 0,
            requested: 0,
            found: 0,
            not_found: 0,
            failed: 0,
            skipped_already_paid: 0,
        }
    }
}

/// Garde-fous du run. (Pas de `Debug` : [`RateLimiter`] n'en a pas, et un
/// limiteur ne se lit pas utilement dans une trace.)
#[derive(Clone, Copy)]
pub struct FillOptions<'a> {
    /// Plafond de requêtes pour **ce** run. Une bibliothèque de 50 000 pistes
    /// ne part pas d'un coup chez un service bénévole ; la passe étant
    /// reprenable, plusieurs runs valent un run illimité.
    pub max_requests: usize,
    /// Limiteur de débit acquis **avant chaque** requête. La production passe
    /// [`crate::http::fetch::LRCLIB`] — le limiteur partagé, pour que deux
    /// passes concurrentes ne doublent pas la cadence. Injecté plutôt que
    /// codé en dur pour que les tests s'exécutent en millisecondes.
    pub limiter: &'a RateLimiter,
}

impl FillOptions<'static> {
    /// Les garde-fous de production : 500 requêtes par run, ~1 req/s,
    /// limiteur partagé avec tout autre appelant LRCLIB.
    pub fn production() -> Self {
        Self {
            max_requests: 500,
            limiter: &crate::http::fetch::LRCLIB,
        }
    }
}

/// Le consentement, lu là où il est stocké. Rendu public pour que la route
/// puisse répondre « refusé » sans dupliquer la règle.
pub fn lrclib_consent_given(db: &Arc<dyn DbBackend>) -> bool {
    crate::db::settings_repo::SettingsRepo::with_backend(db.clone())
        .get(SETTING_LRCLIB_ENABLED)
        .ok()
        .flatten()
        .as_deref()
        == Some("true")
}

/// Remplit les paroles manquantes de la bibliothèque depuis LRCLIB.
///
/// `fetch` est injecté : la production y met
/// [`crate::lyrics::fetch_lrclib_raw`], les tests une fermeture qui ne sort pas
/// de la machine. C'est ce qui permet de **prouver** le consentement et le
/// non-repaiement en comptant les appels, sans jamais bombarder le vrai
/// service.
///
/// Voir l'en-tête du module pour les garanties (consentement, débit, 429,
/// reprise, non-repaiement).
pub async fn run_lrclib_fill<F, Fut>(
    db: &Arc<dyn DbBackend>,
    opts: FillOptions<'_>,
    mut progress: impl FnMut(&FillReport),
    fetch: F,
) -> FillReport
where
    F: Fn(LyricsCandidate) -> Fut,
    Fut: std::future::Future<Output = Result<Option<LrclibRaw>, FetchFailure>>,
{
    let mut report = FillReport::default();

    // Consentement — avant tout, et avant la moindre requête.
    if !lrclib_consent_given(db) {
        info!("lyrics_fill_refused_no_consent");
        report.status = FillStatus::Refused;
        return report;
    }

    let mut consecutive_failures = 0usize;
    let mut after_id = 0i64;
    const BATCH: i64 = 200;

    'outer: loop {
        let batch = match candidates(db, after_id, BATCH) {
            Ok(b) => b,
            Err(e) => {
                warn!(error = %e, "lyrics_fill_candidates_failed");
                break;
            }
        };
        if batch.is_empty() {
            break;
        }

        for cand in batch {
            after_id = cand.track_id;
            report.examined += 1;

            if report.requested >= opts.max_requests {
                report.status = FillStatus::Capped;
                break 'outer;
            }

            // Deuxième garde, la même règle que la route à la demande : la
            // sélection SQL peut dater si l'utilisateur vient d'écouter cette
            // piste. On ne repaie pas.
            if let Some(entry) = crate::lyrics::load_cache_entry(db, cand.track_id)
                && entry.spares_a_fetch()
            {
                report.skipped_already_paid += 1;
                continue;
            }

            // Débit : le limiteur partagé réserve le créneau AVANT la requête.
            // La première part sans attendre si personne n'a appelé LRCLIB
            // récemment ; les suivantes sont espacées d'au moins un créneau.
            opts.limiter.acquire(LIMITER_KEY).await;

            report.requested += 1;
            let track_id = cand.track_id;
            let title = cand.title.clone();
            let artist = cand.artist.clone();
            match fetch(cand).await {
                Ok(raw) => {
                    consecutive_failures = 0;
                    let raw = raw.unwrap_or_default();
                    let empty = raw.is_empty();
                    // Succès **comme** échec sont écrits : c'est la trace de
                    // tentative qui évite de repayer au prochain passage.
                    crate::lyrics::store_cache_entry(
                        db,
                        track_id,
                        &title,
                        &artist,
                        raw.synced_lyrics.as_deref(),
                        raw.plain_lyrics.as_deref(),
                    );
                    if empty {
                        report.not_found += 1;
                    } else {
                        report.found += 1;
                    }
                }
                Err(FetchFailure::RateLimited) => {
                    // Le service a dit « ralentis » : on s'arrête. La reprise
                    // est gratuite, l'insistance ne l'est pas.
                    report.failed += 1;
                    warn!(track_id, "lyrics_fill_stopped_rate_limited");
                    report.status = FillStatus::RateLimited;
                    break 'outer;
                }
                Err(FetchFailure::Transport(e)) => {
                    report.failed += 1;
                    consecutive_failures += 1;
                    debug!(track_id, error = %e, "lyrics_fill_fetch_failed");
                }
            }

            if consecutive_failures >= MAX_CONSECUTIVE_FAILURES {
                warn!(consecutive_failures, "lyrics_fill_stopped_network");
                report.status = FillStatus::NetworkError;
                break 'outer;
            }

            progress(&report);
        }
    }

    info!(
        status = ?report.status,
        examined = report.examined,
        requested = report.requested,
        found = report.found,
        not_found = report.not_found,
        failed = report.failed,
        "lyrics_fill_done"
    );
    progress(&report);
    report
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::artist_repo::ArtistRepo;
    use crate::db::models::{Artist, Track};
    use crate::db::settings_repo::SettingsRepo;
    use crate::db::sqlite::SqliteDb;
    use crate::db::track_metadata_repo::TrackMetadataRepo;
    use crate::db::track_repo::TrackRepo;
    use std::sync::atomic::AtomicUsize;
    use std::time::Duration;

    /// Base complète : `CORE_SCHEMA` **puis** les migrations — `track_metadata`
    /// et `lyrics_cache` n'existent que par migration, et l'indicateur les lit
    /// toutes les deux. (Même montage que `audio::embedding`.)
    fn test_db() -> Arc<dyn DbBackend> {
        let db = SqliteDb::open_in_memory().unwrap();
        db.init_schema().unwrap();
        crate::db::migrations::run_migrations(&db).unwrap();
        Arc::new(db)
    }

    /// Crée une piste avec un artiste réel : `tracks` n'a pas de colonne
    /// `artist_name`, la jointure sur `artists` est la seule source du nom, et
    /// la sélection des candidates l'exige.
    fn make_track(db: &Arc<dyn DbBackend>, title: &str, artist: &str) -> i64 {
        let artist_id = ArtistRepo::with_backend(db.clone())
            .create(&Artist::new(artist.into()))
            .unwrap();
        let mut t = Track::new(title.into());
        t.artist_id = Some(artist_id);
        t.duration_ms = 210_000;
        TrackRepo::with_backend(db.clone()).create(&t).unwrap()
    }

    fn consent(db: &Arc<dyn DbBackend>, on: bool) {
        SettingsRepo::with_backend(db.clone())
            .set(SETTING_LRCLIB_ENABLED, if on { "true" } else { "false" })
            .unwrap();
    }

    fn some_lyrics() -> LrclibRaw {
        LrclibRaw {
            synced_lyrics: Some("[00:10.00] Une ligne\n".into()),
            plain_lyrics: Some("Une ligne".into()),
        }
    }

    // -- L'indicateur ------------------------------------------------------

    #[test]
    fn couverture_nomme_chaque_source_et_ne_compte_personne_deux_fois() {
        let db = test_db();
        let meta = TrackMetadataRepo::with_backend(db.clone());

        let lrc = make_track(&db, "Avec un lrc", "A");
        meta.set(lrc, META_KEY_SIDECAR, "/musique/a.lrc").unwrap();

        let tag = make_track(&db, "Avec une etiquette", "B");
        meta.set(tag, META_KEY_TAG, "Des paroles").unwrap();

        let remote = make_track(&db, "Depuis lrclib", "C");
        crate::lyrics::store_cache_entry(
            &db,
            remote,
            "Depuis lrclib",
            "C",
            Some("[00:01.00] x"),
            None,
        );

        // --- Les trois recouvrements possibles. Chacun teste UN maillon de la
        // précédence `lrc` > `tag` > `lrclib` ; sans les trois, casser un
        // maillon ne ferait rougir personne (trouvé par mutation).

        // `.lrc` + LRCLIB → compte en `lrc`.
        let lrc_et_lrclib = make_track(&db, "lrc et lrclib", "D");
        meta.set(lrc_et_lrclib, META_KEY_SIDECAR, "/musique/d.lrc")
            .unwrap();
        crate::lyrics::store_cache_entry(
            &db,
            lrc_et_lrclib,
            "lrc et lrclib",
            "D",
            Some("[00:01.00] y"),
            None,
        );

        // `.lrc` + étiquette → compte en `lrc`.
        let lrc_et_tag = make_track(&db, "lrc et etiquette", "G");
        meta.set(lrc_et_tag, META_KEY_SIDECAR, "/musique/g.lrc")
            .unwrap();
        meta.set(lrc_et_tag, META_KEY_TAG, "Des paroles").unwrap();

        // étiquette + LRCLIB → compte en `tag`.
        let tag_et_lrclib = make_track(&db, "etiquette et lrclib", "H");
        meta.set(tag_et_lrclib, META_KEY_TAG, "Des paroles")
            .unwrap();
        crate::lyrics::store_cache_entry(
            &db,
            tag_et_lrclib,
            "etiquette et lrclib",
            "H",
            Some("[00:01.00] z"),
            None,
        );

        // Cherchée sans résultat : la trace de tentative.
        let searched = make_track(&db, "Cherchee en vain", "E");
        crate::lyrics::store_cache_entry(&db, searched, "Cherchee en vain", "E", None, None);

        // Jamais cherchée.
        let _never = make_track(&db, "Jamais cherchee", "F");

        let c = coverage(&db).unwrap();
        assert_eq!(c.total_tracks, 8);
        assert_eq!(
            c.from_lrc, 3,
            "`.lrc` prime sur l'étiquette ET sur lrclib : 3 pistes"
        );
        assert_eq!(
            c.from_tag, 2,
            "l'étiquette prime sur lrclib, mais pas sur `.lrc` : 2 pistes"
        );
        assert_eq!(c.from_lrclib, 1, "lrclib ne compte que faute de mieux");
        assert_eq!(
            c.with_lyrics,
            c.from_lrc + c.from_tag + c.from_lrclib,
            "les trois sources doivent s'additionner exactement"
        );
        assert_eq!(c.with_lyrics, 6);
        assert_eq!(c.without_lyrics, 2);
        assert_eq!(c.searched_no_result, 1);
        assert_eq!(c.never_searched, 1);
        assert_eq!(
            c.searched_no_result + c.never_searched,
            c.without_lyrics,
            "toute piste sans paroles est soit déjà cherchée, soit jamais cherchée"
        );
    }

    #[test]
    fn couverture_ignore_les_corps_vides() {
        let db = test_db();
        let meta = TrackMetadataRepo::with_backend(db.clone());
        let t = make_track(&db, "Etiquette vide", "A");
        // Une clé présente mais vide n'est pas des paroles.
        meta.set(t, META_KEY_TAG, "").unwrap();
        meta.set(t, META_KEY_SIDECAR, "").unwrap();

        let c = coverage(&db).unwrap();
        assert_eq!(c.with_lyrics, 0);
        assert_eq!(c.without_lyrics, 1);
        assert_eq!(c.never_searched, 1);
    }

    #[test]
    fn couverture_ignore_le_cache_des_radios() {
        // `lyrics_cache` range aussi les paroles « radio » sous un id négatif
        // (meta_cache_id). Elles ne correspondent à aucune piste et ne doivent
        // pas gonfler l'indicateur.
        let db = test_db();
        make_track(&db, "Une piste", "A");
        let radio_id = crate::lyrics::meta_cache_id("So What", "Miles Davis");
        crate::lyrics::store_cache_entry(
            &db,
            radio_id,
            "So What",
            "Miles Davis",
            Some("[00:01.00] z"),
            None,
        );

        let c = coverage(&db).unwrap();
        assert_eq!(c.total_tracks, 1);
        assert_eq!(c.with_lyrics, 0);
    }

    // -- Sélection des candidates -----------------------------------------

    #[test]
    fn candidates_ecarte_ce_qui_est_deja_paye() {
        let db = test_db();
        let meta = TrackMetadataRepo::with_backend(db.clone());

        let libre = make_track(&db, "A chercher", "A");
        let avec_tag = make_track(&db, "Deja etiquetee", "B");
        meta.set(avec_tag, META_KEY_TAG, "Des paroles").unwrap();
        let avec_lrc = make_track(&db, "Deja en lrc", "C");
        meta.set(avec_lrc, META_KEY_SIDECAR, "/musique/c.lrc")
            .unwrap();
        let deja_trouvee = make_track(&db, "Deja trouvee", "D");
        crate::lyrics::store_cache_entry(
            &db,
            deja_trouvee,
            "Deja trouvee",
            "D",
            Some("[00:01] x"),
            None,
        );
        let echec_frais = make_track(&db, "Echec frais", "E");
        crate::lyrics::store_cache_entry(&db, echec_frais, "Echec frais", "E", None, None);

        let ids: Vec<i64> = candidates(&db, 0, 100)
            .unwrap()
            .into_iter()
            .map(|c| c.track_id)
            .collect();
        assert_eq!(
            ids,
            vec![libre],
            "seule la piste jamais cherchée est candidate ({} candidates examinées)",
            ids.len()
        );
    }

    #[test]
    fn candidates_reprend_un_echec_perime() {
        let db = test_db();
        let t = make_track(&db, "Echec perime", "A");
        crate::lyrics::store_cache_entry(&db, t, "Echec perime", "A", None, None);
        // Vieillit l'échec au-delà de la fenêtre de re-tentative.
        db.execute(
            "UPDATE lyrics_cache SET fetched_at = '2020-01-01T00:00:00Z' WHERE track_id = ?1",
            &[&t],
        )
        .unwrap();

        let ids: Vec<i64> = candidates(&db, 0, 100)
            .unwrap()
            .into_iter()
            .map(|c| c.track_id)
            .collect();
        assert_eq!(ids, vec![t], "un échec périmé redevient candidat");
    }

    #[test]
    fn candidates_ecarte_les_pistes_sans_artiste() {
        let db = test_db();
        // Piste sans artiste : LRCLIB ne peut rien en faire.
        let mut t = Track::new("Orpheline".into());
        t.duration_ms = 1000;
        TrackRepo::with_backend(db.clone()).create(&t).unwrap();
        let ok = make_track(&db, "Avec artiste", "A");

        let ids: Vec<i64> = candidates(&db, 0, 100)
            .unwrap()
            .into_iter()
            .map(|c| c.track_id)
            .collect();
        assert_eq!(ids, vec![ok]);
    }

    // -- La passe LRCLIB ---------------------------------------------------

    /// Limiteur « de test » : même code que la production, intervalle réduit
    /// pour que la suite s'exécute en millisecondes. Local à chaque test, donc
    /// aucun créneau n'est partagé entre tests parallèles.
    fn fast_limiter() -> RateLimiter {
        RateLimiter::with_interval(Duration::from_millis(1))
    }

    fn fast(limiter: &RateLimiter) -> FillOptions<'_> {
        FillOptions {
            max_requests: 100,
            limiter,
        }
    }

    #[tokio::test]
    async fn sans_consentement_aucune_requete_ne_part() {
        let db = test_db();
        make_track(&db, "Une piste", "A");
        consent(&db, false);

        let limiter = fast_limiter();
        let calls = AtomicUsize::new(0);
        let report = run_lrclib_fill(
            &db,
            fast(&limiter),
            |_| {},
            |_c| {
                calls.fetch_add(1, Ordering::Relaxed);
                async { Ok(Some(some_lyrics())) }
            },
        )
        .await;

        assert_eq!(report.status, FillStatus::Refused);
        assert_eq!(
            calls.load(Ordering::Relaxed),
            0,
            "le réglage lyrics_lrclib_enabled gouverne AUSSI la passe de fond"
        );
        assert_eq!(report.requested, 0);
    }

    #[tokio::test]
    async fn avec_consentement_remplit_puis_ne_repaie_plus() {
        let db = test_db();
        for i in 0..3 {
            make_track(&db, &format!("Piste {i}"), &format!("Artiste {i}"));
        }
        consent(&db, true);

        let limiter = fast_limiter();
        let calls = AtomicUsize::new(0);
        let first = run_lrclib_fill(
            &db,
            fast(&limiter),
            |_| {},
            |_c| {
                calls.fetch_add(1, Ordering::Relaxed);
                async { Ok(Some(some_lyrics())) }
            },
        )
        .await;

        assert_eq!(first.status, FillStatus::Done);
        assert_eq!(first.requested, 3);
        assert_eq!(first.found, 3);
        assert_eq!(calls.load(Ordering::Relaxed), 3);
        assert_eq!(coverage(&db).unwrap().from_lrclib, 3);

        // Deuxième passe, immédiatement : tout est déjà payé.
        let second = run_lrclib_fill(
            &db,
            fast(&limiter),
            |_| {},
            |_c| {
                calls.fetch_add(1, Ordering::Relaxed);
                async { Ok(Some(some_lyrics())) }
            },
        )
        .await;
        assert_eq!(second.requested, 0);
        assert_eq!(
            calls.load(Ordering::Relaxed),
            3,
            "aucune requête supplémentaire : les 3 étaient en cache"
        );
    }

    #[tokio::test]
    async fn un_echec_de_recherche_nest_pas_repaye_a_la_passe_suivante() {
        let db = test_db();
        make_track(&db, "Introuvable", "A");
        consent(&db, true);

        let limiter = fast_limiter();
        let calls = AtomicUsize::new(0);
        // LRCLIB répond « je n'ai rien » (404 → Ok(None)).
        let first = run_lrclib_fill(
            &db,
            fast(&limiter),
            |_| {},
            |_c| {
                calls.fetch_add(1, Ordering::Relaxed);
                async { Ok(None) }
            },
        )
        .await;
        assert_eq!(first.requested, 1);
        assert_eq!(first.not_found, 1);
        assert_eq!(first.found, 0);

        // La trace de tentative doit exister ET être visible dans l'indicateur.
        let c = coverage(&db).unwrap();
        assert_eq!(c.with_lyrics, 0);
        assert_eq!(c.searched_no_result, 1);
        assert_eq!(c.never_searched, 0);

        let second = run_lrclib_fill(
            &db,
            fast(&limiter),
            |_| {},
            |_c| {
                calls.fetch_add(1, Ordering::Relaxed);
                async { Ok(None) }
            },
        )
        .await;
        assert_eq!(
            calls.load(Ordering::Relaxed),
            1,
            "une recherche infructueuse ne se repaie pas à chaque passe"
        );
        assert_eq!(second.examined, 0, "la piste n'est même plus candidate");
    }

    /// La sélection SQL prend une tranche de candidates d'un coup. Si
    /// l'utilisateur écoute une de ces pistes pendant que la passe déroule la
    /// tranche, la route à la demande garnit `lyrics_cache` — et la passe ne
    /// doit pas repayer cette requête-là.
    ///
    /// Simulé exactement : la fermeture de récupération garnit le cache de la
    /// piste suivante, comme le ferait la route.
    #[tokio::test]
    async fn une_piste_payee_pendant_la_passe_nest_pas_repayee() {
        let db = test_db();
        let premiere = make_track(&db, "Premiere", "A");
        let seconde = make_track(&db, "Seconde", "B");
        consent(&db, true);
        assert_eq!(
            candidates(&db, 0, 100).unwrap().len(),
            2,
            "les deux pistes sont bien candidates au départ"
        );

        let limiter = fast_limiter();
        let calls = AtomicUsize::new(0);
        let db_for_fetch = db.clone();
        let report = run_lrclib_fill(
            &db,
            fast(&limiter),
            |_| {},
            |c: LyricsCandidate| {
                calls.fetch_add(1, Ordering::Relaxed);
                if c.track_id == premiere {
                    // Ce que fait la route à la demande quand on écoute `seconde`.
                    crate::lyrics::store_cache_entry(
                        &db_for_fetch,
                        seconde,
                        "Seconde",
                        "B",
                        Some("[00:02.00] deja la"),
                        None,
                    );
                }
                async { Ok(Some(some_lyrics())) }
            },
        )
        .await;

        assert_eq!(
            calls.load(Ordering::Relaxed),
            1,
            "la seconde piste avait été payée entre-temps : pas de requête pour elle"
        );
        assert_eq!(report.requested, 1);
        assert_eq!(report.skipped_already_paid, 1);
        assert_eq!(report.examined, 2);
    }

    #[tokio::test]
    async fn le_plafond_de_requetes_est_tenu_et_la_reprise_continue() {
        let db = test_db();
        for i in 0..5 {
            make_track(&db, &format!("Piste {i}"), &format!("Artiste {i}"));
        }
        consent(&db, true);

        let limiter = fast_limiter();
        let opts = FillOptions {
            max_requests: 2,
            limiter: &limiter,
        };
        let calls = AtomicUsize::new(0);
        let first = run_lrclib_fill(
            &db,
            opts,
            |_| {},
            |_c| {
                calls.fetch_add(1, Ordering::Relaxed);
                async { Ok(Some(some_lyrics())) }
            },
        )
        .await;
        assert_eq!(first.status, FillStatus::Capped);
        assert_eq!(first.requested, 2);
        assert_eq!(calls.load(Ordering::Relaxed), 2);

        // Reprise : les 3 restantes, sans repayer les 2 premières.
        let second = run_lrclib_fill(
            &db,
            fast(&limiter),
            |_| {},
            |_c| {
                calls.fetch_add(1, Ordering::Relaxed);
                async { Ok(Some(some_lyrics())) }
            },
        )
        .await;
        assert_eq!(second.status, FillStatus::Done);
        assert_eq!(second.requested, 3);
        assert_eq!(calls.load(Ordering::Relaxed), 5);
        assert_eq!(coverage(&db).unwrap().from_lrclib, 5);
    }

    #[tokio::test]
    async fn le_debit_est_respecte_avant_chaque_requete() {
        let db = test_db();
        for i in 0..4 {
            make_track(&db, &format!("Piste {i}"), &format!("Artiste {i}"));
        }
        consent(&db, true);

        let limiter = RateLimiter::with_interval(Duration::from_millis(40));
        let opts = FillOptions {
            max_requests: 100,
            limiter: &limiter,
        };
        let start = std::time::Instant::now();
        let report = run_lrclib_fill(&db, opts, |_| {}, |_c| async { Ok(None) }).await;
        let elapsed = start.elapsed();

        // Le limiteur espace les requêtes : la première part tout de suite,
        // les trois suivantes une par créneau de 40 ms. Sans lui, les quatre
        // partiraient d'un bloc et le temps écoulé serait quasi nul.
        assert_eq!(report.requested, 4);
        assert!(
            elapsed >= Duration::from_millis(120),
            "4 requêtes espacées de 40 ms : {elapsed:?} < 120 ms"
        );
    }

    #[tokio::test]
    async fn un_429_arrete_la_passe_immediatement() {
        let db = test_db();
        for i in 0..5 {
            make_track(&db, &format!("Piste {i}"), &format!("Artiste {i}"));
        }
        consent(&db, true);

        let limiter = fast_limiter();
        let calls = AtomicUsize::new(0);
        let report = run_lrclib_fill(
            &db,
            fast(&limiter),
            |_| {},
            |_c| {
                calls.fetch_add(1, Ordering::Relaxed);
                async { Err(FetchFailure::RateLimited) }
            },
        )
        .await;

        assert_eq!(report.status, FillStatus::RateLimited);
        assert_eq!(
            calls.load(Ordering::Relaxed),
            1,
            "au premier « ralentis », on s'arrête — pas 5 requêtes"
        );
    }

    #[tokio::test]
    async fn les_echecs_reseau_consecutifs_arretent_la_passe() {
        let db = test_db();
        for i in 0..10 {
            make_track(&db, &format!("Piste {i}"), &format!("Artiste {i}"));
        }
        consent(&db, true);

        let limiter = fast_limiter();
        let calls = AtomicUsize::new(0);
        let report = run_lrclib_fill(
            &db,
            fast(&limiter),
            |_| {},
            |_c| {
                calls.fetch_add(1, Ordering::Relaxed);
                async { Err(FetchFailure::Transport("connection refused".into())) }
            },
        )
        .await;

        assert_eq!(report.status, FillStatus::NetworkError);
        assert_eq!(calls.load(Ordering::Relaxed), MAX_CONSECUTIVE_FAILURES);
        // Rien mis en cache sur panne réseau : la prochaine passe retentera.
        assert_eq!(coverage(&db).unwrap().searched_no_result, 0);
    }

    // -- La passe locale ---------------------------------------------------

    #[test]
    fn la_passe_locale_inscrit_les_lrc_voisins_et_lindicateur_les_voit() {
        let dir = tempfile::tempdir().unwrap();
        let audio = dir.path().join("morceau.flac");
        std::fs::write(&audio, b"pas vraiment du flac").unwrap();
        std::fs::write(dir.path().join("morceau.lrc"), "[00:10.00] Une ligne\n").unwrap();

        let vide = dir.path().join("vide.flac");
        std::fs::write(&vide, b"x").unwrap();
        std::fs::write(dir.path().join("vide.lrc"), "   \n").unwrap();

        let db = test_db();
        let repo = TrackRepo::with_backend(db.clone());
        let artist_id = ArtistRepo::with_backend(db.clone())
            .create(&Artist::new("A".into()))
            .unwrap();
        for (title, path) in [("Morceau", &audio), ("Vide", &vide)] {
            let mut t = Track::new(title.into());
            t.artist_id = Some(artist_id);
            t.file_path = Some(path.to_string_lossy().to_string());
            repo.create(&t).unwrap();
        }

        assert_eq!(coverage(&db).unwrap().from_lrc, 0, "avant la passe");

        let report = run_local_index(&db, 2, |_, _| {});
        assert_eq!(report.examined, 2);
        assert_eq!(report.found, 1, "le .lrc vide ne compte pas");

        let c = coverage(&db).unwrap();
        assert_eq!(c.from_lrc, 1, "après la passe, l'indicateur les voit");
        assert_eq!(c.without_lyrics, 1);
    }

    #[test]
    fn la_passe_locale_nappelle_pas_deux_fois_les_memes_pistes() {
        let dir = tempfile::tempdir().unwrap();
        let audio = dir.path().join("morceau.flac");
        std::fs::write(&audio, b"x").unwrap();
        std::fs::write(dir.path().join("morceau.lrc"), "[00:10.00] Une ligne\n").unwrap();

        let db = test_db();
        let artist_id = ArtistRepo::with_backend(db.clone())
            .create(&Artist::new("A".into()))
            .unwrap();
        let mut t = Track::new("Morceau".into());
        t.artist_id = Some(artist_id);
        t.file_path = Some(audio.to_string_lossy().to_string());
        TrackRepo::with_backend(db.clone()).create(&t).unwrap();

        assert_eq!(run_local_index(&db, 1, |_, _| {}).found, 1);
        let again = run_local_index(&db, 1, |_, _| {});
        assert_eq!(
            again.examined, 0,
            "une piste dont le .lrc est déjà inscrit n'est pas ré-examinée"
        );
    }
}
