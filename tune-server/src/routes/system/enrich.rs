use axum::Json;
use axum::extract::State;
use axum::http::HeaderMap;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use serde_json::{Value, json};
use tune_http_types::panne_sql::OuDefautJournalise;

use tune_core::db::album_repo::AlbumRepo;
use tune_core::db::artist_repo::ArtistRepo;
use tune_core::db::settings_repo::SettingsRepo;
use tune_core::db::track_repo::TrackRepo;
use tune_core::license::Feature;

use crate::error::AppError;
use crate::state::AppState;

/// GET /system/background-tasks — current in-progress background tasks
/// (enrichment, artwork, bios) for the UI indicator. The live truth is pushed
/// over the `system.background_tasks` WebSocket event; this endpoint provides
/// the initial snapshot for a client that connects mid-task.
pub(super) async fn background_tasks_status(State(state): State<AppState>) -> Json<Value> {
    Json(json!({ "tasks": state.background_tasks.snapshot() }))
}

// ---------------------------------------------------------------------------
// Free-tier daily enrichment limit
// ---------------------------------------------------------------------------

const FREE_DAILY_ENRICHMENT_LIMIT: i64 = 10;
const ENRICHMENT_COUNT_KEY: &str = "enrichment_daily_count";
const ENRICHMENT_DATE_KEY: &str = "enrichment_daily_date";

/// Returns (count_used_today, limit). Resets counter if the date has changed.
fn get_daily_enrichment_usage(settings: &SettingsRepo) -> (i64, i64) {
    let today = today_utc_str();
    let stored_date = settings
        .get(ENRICHMENT_DATE_KEY)
        .ok()
        .flatten()
        .unwrap_or_default();

    if stored_date != today {
        // New day — reset counter
        settings.set(ENRICHMENT_DATE_KEY, &today).ok();
        settings.set(ENRICHMENT_COUNT_KEY, "0").ok();
        return (0, FREE_DAILY_ENRICHMENT_LIMIT);
    }

    let count: i64 = settings
        .get(ENRICHMENT_COUNT_KEY)
        .ok()
        .flatten()
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);

    (count, FREE_DAILY_ENRICHMENT_LIMIT)
}

/// Increment the daily enrichment counter by `n`.
fn increment_daily_enrichment(settings: &SettingsRepo, n: i64) {
    let (current, _) = get_daily_enrichment_usage(settings);
    let new_count = current + n;
    settings
        .set(ENRICHMENT_COUNT_KEY, &new_count.to_string())
        .ok();
}

// ---------------------------------------------------------------------------
// POST /system/enrich — artwork enrichment (MBID + covers)
// ---------------------------------------------------------------------------

/// The daily free-tier enrichment gate shared by every `/system/enrich*`
/// mutation route: premium runs unlimited, the free tier consumes one of its
/// daily quota and gets a `429` body once it is spent. Returns the
/// `(is_premium, settings)` the caller needs on allow, or `Err(response)` to
/// short-circuit with the quota error. Centralises four identical copies of
/// this block — the single place to reason about the enrichment quota.
pub(crate) async fn gate_enrichment(state: &AppState) -> Result<bool, (StatusCode, Json<Value>)> {
    let is_premium = state.license.check_feature(Feature::AutoEnrichment).await;
    let settings = SettingsRepo::with_backend(state.backend.clone());
    if !is_premium {
        let (used, limit) = get_daily_enrichment_usage(&settings);
        if used >= limit {
            return Err((
                StatusCode::TOO_MANY_REQUESTS,
                Json(json!({
                    "error": "free_tier_daily_enrichment_limit_reached",
                    "used": used,
                    "limit": limit,
                    "upgrade": "Premium unlocks unlimited auto enrichment",
                })),
            ));
        }
        increment_daily_enrichment(&settings, 1);
    }
    Ok(is_premium)
}

pub(super) async fn system_enrich(State(state): State<AppState>) -> impl IntoResponse {
    let is_premium = match gate_enrichment(&state).await {
        Ok(p) => p,
        Err(resp) => return resp,
    };

    let db = state.backend.clone();
    let cache_dir = crate::routes::library::artwork_cache_dir();
    let artist_cache_dir = cache_dir.clone();
    tokio::spawn(async move {
        tune_core::library::artwork::batch_enrich_artwork(db, cache_dir).await;
    });
    let mbid_db = state.backend.clone();
    let art_db = state.backend.clone();
    let art_cache = artist_cache_dir.clone();
    tokio::spawn(async move {
        // 1. Match MusicBrainz IDs for artists without one
        tokio::time::sleep(std::time::Duration::from_secs(5)).await;
        tune_core::metadata::matcher::batch_match_artist_mbids(mbid_db).await;
        // 2. Fetch images for artists with MBID
        tokio::time::sleep(std::time::Duration::from_secs(3)).await;
        tune_core::library::artwork::batch_enrich_artist_artwork(art_db, art_cache).await;
    });
    (
        StatusCode::ACCEPTED,
        Json(json!({
            "status": "enrichment_started",
            "premium": is_premium,
        })),
    )
}

// ---------------------------------------------------------------------------
// Le fonds communautaire est indexé par MBID (#2258)
// ---------------------------------------------------------------------------

/// Ce que la clé du fonds communautaire de biographies écarte, dit à
/// l'utilisateur (#2258).
///
/// `cloud::bio_sync::download_artist_bios` interroge mozaiklabs.fr par
/// `?musicbrainz_ids=…` : **le fonds est indexé par MBID**. Les deux requêtes
/// qui l'alimentent — envoi et candidats au téléchargement — exigent donc un
/// MBID non vide, et un artiste qui n'en a pas est écarté des deux côtés.
///
/// Ce n'est pas un défaut de ces requêtes : c'est la conséquence d'un choix de
/// clé, et la lever enverrait des identifiants vides à une API qui s'en sert
/// d'index. Ce qui était un défaut, c'est que cette exclusion soit
/// **totalement silencieuse** : le testeur voyait cent vingt fiches vides sans
/// pouvoir distinguer une panne, une source avare et une clé qu'il ne possède
/// pas.
///
/// Deux nombres, donc, et le nom de la clé qui les produit. Le remède est
/// nommé lui aussi : `batch_match_artist_mbids` existe déjà et tourne dans
/// `POST /system/enrich` comme dans `POST /system/enrichment/run` — il
/// n'accepte un appariement par le nom qu'au-dessus d'un score MusicBrainz de
/// 90 (`metadata::matcher::lookup_artist`), précisément parce qu'un mauvais
/// MBID rattacherait une biographie étrangère.
///
/// ⚠ À ne pas confondre avec `bio_last_run.artists.sans_source`, qui compte
/// les artistes que les sources LOCALES ne peuvent pas servir (ni MBID ni clé
/// Last.fm). Poser une clé Last.fm remet celui-là à zéro et ne change rien à
/// celui-ci.
fn fonds_communautaire(artist_repo: &ArtistRepo) -> Value {
    let hors = artist_repo.hors_fonds_communautaire().unwrap_or_default();
    json!({
        "cle": "musicbrainz_id",
        "bios_non_partagees": hors.bios_non_partagees,
        "artistes_non_servis": hors.artistes_non_servis,
        "remede": "artist_mbid_matching",
    })
}

// ---------------------------------------------------------------------------
// POST /system/enrich-bios — bio enrichment
// ---------------------------------------------------------------------------

pub(super) async fn enrich_bios(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> impl IntoResponse {
    let lang = crate::i18n::lang_from_header(&headers);
    let is_premium = match gate_enrichment(&state).await {
        Ok(p) => p,
        Err(resp) => return resp,
    };

    let artist_db = state.backend.clone();
    let album_db = state.backend.clone();

    let artist_repo = ArtistRepo::with_backend(state.backend.clone());
    let album_repo = AlbumRepo::with_backend(state.backend.clone());
    let without_artist_bio = artist_repo.list_without_bio().unwrap_or_default().len();
    let without_album_bio = album_repo.list_without_bio().unwrap_or_default().len();
    // Le compte est pris AVANT que les passes soient lancées : il décrit la
    // bibliothèque telle que l'utilisateur vient de la soumettre.
    let hors_fonds = fonds_communautaire(&artist_repo);

    // One task registered for both bio passes; it clears when the last of the
    // two spawned futures drops its Arc clone of the guard.
    let task_guard = std::sync::Arc::new(state.background_tasks.begin(
        "bios",
        "Récupération des biographies…",
        "enrichment",
    ));

    let lang_artist = lang.clone();
    let guard_artist = task_guard.clone();
    tokio::spawn(async move {
        let _guard = guard_artist;
        tune_core::metadata::bio_batch::batch_enrich_artist_bios(artist_db, &lang_artist).await;
    });
    let guard_album = task_guard;
    tokio::spawn(async move {
        let _guard = guard_album;
        tokio::time::sleep(std::time::Duration::from_secs(5)).await;
        tune_core::metadata::bio_batch::batch_enrich_album_bios(album_db, &lang).await;
    });

    (
        StatusCode::ACCEPTED,
        Json(json!({
            "status": "bio_enrichment_started",
            "artists_without_bio": without_artist_bio,
            "albums_without_bio": without_album_bio,
            "premium": is_premium,
            // #2258 — une part de `artists_without_bio` ne peut RIEN attendre
            // du fonds communautaire, faute de MBID. Le dire ici, dans la
            // réponse même du bouton, plutôt que de laisser l'utilisateur
            // conclure d'un écran vide.
            "fonds_communautaire": hors_fonds,
        })),
    )
}

// ---------------------------------------------------------------------------
// POST /system/enrich-metadata — extended file metadata extraction
// ---------------------------------------------------------------------------

pub(super) async fn enrich_extended_metadata(State(state): State<AppState>) -> impl IntoResponse {
    let is_premium = match gate_enrichment(&state).await {
        Ok(p) => p,
        Err(resp) => return resp,
    };

    let db = state.backend.clone();
    tokio::spawn(async move {
        let meta_repo =
            tune_core::db::track_metadata_repo::TrackMetadataRepo::with_backend(db.clone());
        let tracks: Vec<(i64, String)> = db
            .query_many(
                "SELECT id, file_path FROM tracks WHERE file_path IS NOT NULL AND source = 'local'",
                &[],
            )
            .ou_defaut_journalise()
            .into_iter()
            .filter_map(|cols| {
                let id = cols.first()?.as_i64()?;
                let path = cols.get(1)?.as_string()?;
                Some((id, path))
            })
            .collect();
        let total = tracks.len();
        tracing::info!(total, "enrich_extended_metadata_started");
        let mut enriched = 0u64;
        let mut batch: Vec<(i64, std::collections::HashMap<String, String>)> = Vec::new();
        for (track_id, file_path) in &tracks {
            // Le `!path.exists()` nu sautait en silence toute piste dont le nom
            // est en NFD sur le disque alors que la base le tient en NFC
            // (#1865). On resout la graphie que le systeme reconnait, et c'est
            // celle-la qu'on lit — la base n'est pas reecrite.
            let Some(sur_disque) =
                tune_core::library::local_path::resolve_existing_local_path(file_path)
            else {
                continue;
            };
            let path = std::path::Path::new(&sur_disque);
            let ext =
                tokio::task::block_in_place(|| tune_core::metadata::read_extended_metadata(path));
            if !ext.is_empty() {
                batch.push((*track_id, ext));
                enriched += 1;
            }
            if batch.len() >= 500 {
                if let Err(e) = meta_repo.set_batch_multi(&batch) {
                    tracing::error!(error = %e, "enrich_metadata_batch_failed");
                }
                batch.clear();
            }
        }
        if !batch.is_empty() {
            if let Err(e) = meta_repo.set_batch_multi(&batch) {
                tracing::error!(error = %e, "enrich_metadata_batch_failed");
            }
        }
        tracing::info!(total, enriched, "enrich_extended_metadata_complete");
    });
    (
        StatusCode::ACCEPTED,
        Json(json!({
            "status": "extended_metadata_enrichment_started",
            "premium": is_premium,
        })),
    )
}

// ---------------------------------------------------------------------------
// GET /system/enrichment/status — enrichment statistics
// ---------------------------------------------------------------------------

pub(super) async fn enrichment_status(State(state): State<AppState>) -> Json<Value> {
    let is_premium = state.license.check_feature(Feature::AutoEnrichment).await;

    let settings = SettingsRepo::with_backend(state.backend.clone());
    let (daily_used, daily_limit) = get_daily_enrichment_usage(&settings);

    let artist_repo = ArtistRepo::with_backend(state.backend.clone());
    let album_repo = AlbumRepo::with_backend(state.backend.clone());
    let track_repo = TrackRepo::with_backend(state.backend.clone());

    let total_tracks = track_repo.count().unwrap_or(0);
    let total_artists = artist_repo.count().unwrap_or(0);
    let total_albums = album_repo.count().unwrap_or(0);

    // Artists with bios — par soustraction, ce chiffre ne vaut que ce que vaut
    // la requête retranchée.
    //
    // `list_without_bio` exigeait un identifiant MusicBrainz non vide : tout
    // artiste sans MBID sortait de `v` et se retrouvait donc compté ICI, du
    // côté des artistes « pourvus d'une biographie ». Avec 0,9 % de couverture
    // MBID mesurée sur une bibliothèque réelle, le panneau annonçait ~99 % de
    // biographies devant des fiches vides (#1311). La requête ne filtre plus
    // sur le MBID ; ce calcul devient exact sans changer de forme.
    let artists_with_bio = artist_repo
        .list_without_bio()
        .map(|v| total_artists - v.len() as i64)
        .unwrap_or(0);
    // Artists with images — au sens de l'utilisateur : une image QUI S'AFFICHE.
    //
    // La colonne seule ment dès que le cache d'images a été vidé ou déplacé :
    // la base garde des chemins vers des fichiers disparus, et le panneau
    // annonçait « tous les artistes ont une vignette » devant une grille vide
    // (Fabien, 11/08/2026). On retire donc les chemins qui ne pointent plus
    // sur rien, comme le fait déjà l'enrichissement lui-même.
    let artist_repo_paths = ArtistRepo::with_backend(state.backend.clone());
    let artwork_cache = crate::routes::library::artwork_cache_dir();
    let artists_with_image: i64 = artist_repo_paths
        .list_with_image_and_mbid()
        .unwrap_or_default()
        .into_iter()
        .filter(|(_, _, _, image_path)| {
            tune_core::library::artwork::cached_artwork_exists(&artwork_cache, image_path)
        })
        .count() as i64;
    // Albums with covers
    let albums_with_cover: i64 = state
        .backend
        .query_one(
            "SELECT COUNT(*) FROM albums WHERE cover_path IS NOT NULL AND cover_path != ''",
            &[],
        )
        .ok()
        .flatten()
        .and_then(|r| r[0].as_i64())
        .unwrap_or(0);
    // Albums with bios
    let albums_with_bio = album_repo
        .list_without_bio()
        .map(|v| total_albums - v.len() as i64)
        .unwrap_or(0);
    // Artists with MusicBrainz IDs
    let artists_with_mbid: i64 = state
        .backend
        .query_one(
            "SELECT COUNT(*) FROM artists WHERE musicbrainz_id IS NOT NULL AND musicbrainz_id != ''",
            &[],
        )
        .ok()
        .flatten()
        .and_then(|r| r[0].as_i64())
        .unwrap_or(0);

    // Last enrichment run timestamp
    let last_run = settings.get("enrichment_last_run").ok().flatten();

    // Le bilan des deux passes de biographies (#1311).
    //
    // `bio_batch` rangeait déjà ces deux clés à la fin de chaque passe, et
    // **personne ne les relisait** : une recherche de `artist_bio_enrich_result`
    // dans tout le dépôt ne rendait que la ligne de l'écriture. Le serveur
    // savait donc dire pourquoi une passe était rentrée à vide, et ne le disait
    // à personne — c'est le vrai défaut derrière « les bios ne sont pas
    // disponibles » : pas un décompte faux, une absence de retour.
    //
    // Les voici, sous une clé qui leur est propre pour ne rien déplacer de ce
    // que `stats` promet déjà.
    let bio_last_run = json!({
        "artists": bilan_bio(&settings, "artist_bio_enrich_result"),
        "albums": bilan_bio(&settings, "album_bio_enrich_result"),
    });

    Json(json!({
        "premium": is_premium,
        "daily_used": daily_used,
        "daily_limit": if is_premium { null_i64() } else { Some(daily_limit) },
        "stats": {
            "total_tracks": total_tracks,
            "total_artists": total_artists,
            "total_albums": total_albums,
            "artists_with_bio": artists_with_bio,
            "artists_with_image": artists_with_image,
            "artists_with_mbid": artists_with_mbid,
            "albums_with_cover": albums_with_cover,
            "albums_with_bio": albums_with_bio,
        },
        "last_run": last_run,
        "bio_last_run": bio_last_run,
        // #2258 — la part de la bibliothèque que la clé du fonds
        // communautaire écarte, des deux côtés. Sous une clé propre : ce n'est
        // pas un décompte de bibliothèque comme ceux de `stats`, c'est la
        // mesure d'une exclusion et le nom de sa cause.
        "fonds_communautaire": fonds_communautaire(&artist_repo),
    }))
}

/// Le bilan de la dernière passe de biographies rangé sous `cle`, tel que
/// `tune_core::metadata::bio_batch::bilan_de_passe` l'a écrit.
///
/// Rend `null` quand la clé est absente (aucune passe n'a encore tourné) ou
/// quand sa valeur n'est pas du JSON lisible : un bilan illisible ne doit pas
/// faire tomber tout le panneau d'enrichissement, qui porte aussi les
/// décomptes de la bibliothèque.
fn bilan_bio(settings: &SettingsRepo, cle: &str) -> Value {
    settings
        .get(cle)
        .ok()
        .flatten()
        .and_then(|brut| serde_json::from_str::<Value>(&brut).ok())
        .unwrap_or(Value::Null)
}

/// Helper to produce a JSON null for the daily_limit field on Premium.
fn null_i64() -> Option<i64> {
    None
}

/// Return today's date as "YYYY-MM-DD" in UTC, without chrono dependency.
fn today_utc_str() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    // 86400 seconds per day; compute days since epoch and derive date components
    let days = secs / 86400;
    // Civil date from days since 1970-01-01 (Algorithm from Howard Hinnant)
    let z = days as i64 + 719468;
    let era = if z >= 0 { z } else { z - 146096 } / 146097;
    let doe = (z - era * 146097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!("{y:04}-{m:02}-{d:02}")
}

/// Return current UTC timestamp as ISO 8601, without chrono dependency.
fn now_utc_str() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let date = today_utc_str();
    let day_secs = secs % 86400;
    let h = day_secs / 3600;
    let m = (day_secs % 3600) / 60;
    let s = day_secs % 60;
    format!("{date}T{h:02}:{m:02}:{s:02}Z")
}

// ---------------------------------------------------------------------------
// POST /system/enrichment/run — trigger full enrichment run
// ---------------------------------------------------------------------------

/// Corps optionnel de `POST /system/enrichment/run`. Sans corps (ou sans
/// `path`), la passe couvre toute la bibliothèque — contrat historique.
#[derive(serde::Deserialize, Default)]
pub(super) struct EnrichmentRunBody {
    /// Répertoire (sous une racine musicale) auquel limiter la passe (#1660).
    pub(super) path: Option<String>,
}

/// Résout et valide le `path` demandé : normalisation, refus de toute
/// composante `..`, et appartenance à une racine musicale configurée — même
/// garde que le scan ciblé, mais en REFUS franc plutôt qu'en repli silencieux :
/// ici un repli enrichirait toute la bibliothèque, exactement ce que
/// l'utilisateur demandait d'éviter (#1660).
///
/// `pub(crate)` : `/library/enrich-all` — la route que le bouton
/// « Enrichir les métadonnées » de `SettingsView.svelte` appelle réellement —
/// valide son `path` avec CETTE fonction, pas une copie.
pub(crate) fn resoudre_portee(
    state: &AppState,
    path: &str,
) -> Result<tune_core::metadata::enrich_scope::EnrichScope, (StatusCode, Json<Value>)> {
    let dir = tune_core::scanner::walker::normalize_path(path);
    if dir.is_empty() || dir.split(['/', '\\']).any(|c| c == "..") {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": "invalid_path", "path": path })),
        ));
    }
    let music_dirs: Vec<String> = super::get_music_dirs_list(&state.backend)
        .iter()
        .map(|d| tune_core::scanner::walker::normalize_path(d))
        .filter(|d| !d.is_empty())
        .collect();
    if !music_dirs
        .iter()
        .any(|root| super::scan::sous_le_dossier(&dir, root))
    {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(json!({
                "error": "path_outside_music_dirs",
                "path": dir,
                "music_dirs": music_dirs,
            })),
        ));
    }
    Ok(tune_core::metadata::enrich_scope::EnrichScope::from_directory(&state.backend, &dir))
}

/// Attend que TOUTES les passes d'une exécution soient retombées, puis annonce
/// la fin une fois et une seule.
///
/// Le nom de l'événement est un contrat INTER-DÉPÔTS : c'est le client qui
/// écoutait `library.enrich.completed` en premier. Il passe donc par
/// `EventType::EnrichComplete`, verrouillé par `as_str_matches_wire_contract`
/// dans `tune-core/src/event_types.rs` — jamais par une chaîne libre.
///
/// Une passe qui panique ne retient PAS l'annonce : les autres ont travaillé,
/// la bibliothèque a bougé, et l'écran doit relire ses compteurs de toute
/// façon. C'est pourquoi le résultat du `await` est délibérément ignoré.
async fn annoncer_fin_de_passe(
    taches: Vec<tokio::task::JoinHandle<()>>,
    event_bus: std::sync::Arc<tune_core::event_bus::EventBus>,
    charge: Value,
) {
    for tache in taches {
        if let Err(e) = tache.await {
            tracing::warn!(error = %e, "enrichment_run_passe_interrompue");
        }
    }
    event_bus.emit_typed(tune_core::event_types::EventType::EnrichComplete, charge);
    tracing::info!("enrichment_run_completed_event_emitted");
}

pub(super) async fn enrichment_run(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Option<Json<EnrichmentRunBody>>,
) -> impl IntoResponse {
    let bio_lang = crate::i18n::lang_from_header(&headers);

    // Portée par répertoire (#1660) — validée AVANT le gate de quota : un
    // chemin invalide ne consomme rien.
    let scope = match body
        .and_then(|Json(b)| b.path)
        .filter(|p| !p.trim().is_empty())
    {
        Some(p) => match resoudre_portee(&state, &p) {
            Ok(s) => Some(s),
            Err(resp) => return resp,
        },
        None => None,
    };

    let is_premium = match gate_enrichment(&state).await {
        Ok(p) => p,
        Err(resp) => return resp,
    };

    // Record the run timestamp — passe COMPLÈTE seulement : une passe limitée
    // à un répertoire ne doit pas faire dire au panneau Métadonnées que toute
    // la bibliothèque vient d'être traitée.
    let settings = SettingsRepo::with_backend(state.backend.clone());
    if scope.is_none() {
        let now = now_utc_str();
        settings.set("enrichment_last_run", &now).ok();
    }

    // 1. Artwork enrichment
    let db1 = state.backend.clone();
    let cache_dir = crate::routes::library::artwork_cache_dir();
    let cache_dir2 = cache_dir.clone();
    let scope1 = scope.clone();
    let tache_pochettes = tokio::spawn(async move {
        tune_core::library::artwork::batch_enrich_artwork_scoped(db1, cache_dir, scope1).await;
    });

    // 2. Artist MBID matching + artist artwork
    let mbid_db = state.backend.clone();
    let art_db = state.backend.clone();
    let art_cache = cache_dir2.clone();
    let scope_mbid = scope.clone();
    let scope_art = scope.clone();
    let tache_artistes = tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_secs(3)).await;
        tune_core::metadata::matcher::batch_match_artist_mbids_scoped(mbid_db, scope_mbid).await;
        tokio::time::sleep(std::time::Duration::from_secs(2)).await;
        tune_core::library::artwork::batch_enrich_artist_artwork_scoped(
            art_db, art_cache, scope_art,
        )
        .await;
    });

    // 3. Bio enrichment
    let bio_artist_db = state.backend.clone();
    let bio_album_db = state.backend.clone();
    let scope_bio_artist = scope.clone();
    let scope_bio_album = scope.clone();
    let tache_bios = tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_secs(8)).await;
        tune_core::metadata::bio_batch::batch_enrich_artist_bios_scoped(
            bio_artist_db,
            &bio_lang,
            scope_bio_artist,
        )
        .await;
        tokio::time::sleep(std::time::Duration::from_secs(3)).await;
        tune_core::metadata::bio_batch::batch_enrich_album_bios_scoped(
            bio_album_db,
            &bio_lang,
            scope_bio_album,
        )
        .await;
    });

    // 4. Extended file metadata
    let ext_db = state.backend.clone();
    let scope_ext = scope.clone();
    let tache_metadonnees = tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_secs(15)).await;
        let meta_repo =
            tune_core::db::track_metadata_repo::TrackMetadataRepo::with_backend(ext_db.clone());
        let tracks: Vec<(i64, String)> = ext_db
            .query_many(
                "SELECT id, file_path FROM tracks WHERE file_path IS NOT NULL AND source = 'local'",
                &[],
            )
            .ou_defaut_journalise()
            .into_iter()
            .filter_map(|cols| {
                let id = cols.first()?.as_i64()?;
                let path = cols.get(1)?.as_string()?;
                // Portée par répertoire (#1660) : les pistes hors du
                // répertoire demandé ne sont pas candidates.
                if scope_ext
                    .as_ref()
                    .is_some_and(|s| !s.contient_chemin(&path))
                {
                    return None;
                }
                Some((id, path))
            })
            .collect();
        let total = tracks.len();
        tracing::info!(total, "enrichment_run_extended_metadata_started");
        let mut enriched = 0u64;
        let mut batch: Vec<(i64, std::collections::HashMap<String, String>)> = Vec::new();
        for (track_id, file_path) in &tracks {
            // Le `!path.exists()` nu sautait en silence toute piste dont le nom
            // est en NFD sur le disque alors que la base le tient en NFC
            // (#1865). On resout la graphie que le systeme reconnait, et c'est
            // celle-la qu'on lit — la base n'est pas reecrite.
            let Some(sur_disque) =
                tune_core::library::local_path::resolve_existing_local_path(file_path)
            else {
                continue;
            };
            let path = std::path::Path::new(&sur_disque);
            let ext =
                tokio::task::block_in_place(|| tune_core::metadata::read_extended_metadata(path));
            if !ext.is_empty() {
                batch.push((*track_id, ext));
                enriched += 1;
            }
            if batch.len() >= 500 {
                if let Err(e) = meta_repo.set_batch_multi(&batch) {
                    tracing::error!(error = %e, "enrichment_run_metadata_batch_failed");
                }
                batch.clear();
            }
        }
        if !batch.is_empty() {
            if let Err(e) = meta_repo.set_batch_multi(&batch) {
                tracing::error!(error = %e, "enrichment_run_metadata_batch_failed");
            }
        }
        tracing::info!(total, enriched, "enrichment_run_extended_metadata_complete");
    });

    // 5. Point d'achèvement UNIQUE de la passe (#2259).
    //
    // Les quatre passes ci-dessus tournent en parallèle et ne rendent aucun
    // compte : la route répondait 202 puis n'émettait plus rien, alors que le
    // client écoute `library.enrich.completed` depuis la v0.8
    // (`MetadataView.svelte`, `SettingsView.svelte`). #2543 a réparé l'AUTRE
    // chemin d'enrichissement (`/library/enrich-all`) et a laissé celui-ci de
    // côté, faute justement d'un instant « c'est fini » : deux tâches
    // concurrentes, aucun point de jonction. Ce superviseur est ce point —
    // une seule source de vérité, sur le modèle du rapport de fin de scan
    // (#2827), plutôt qu'une émission par passe qui ferait clignoter l'écran
    // quatre fois et annoncerait « terminé » trois fois trop tôt.
    let bus_fin = state.event_bus.clone();
    let repertoire = scope.as_ref().map(|s| s.dir.clone());
    tokio::spawn(async move {
        annoncer_fin_de_passe(
            vec![
                tache_pochettes,
                tache_artistes,
                tache_bios,
                tache_metadonnees,
            ],
            bus_fin,
            json!({ "directory": repertoire }),
        )
        .await;
    });

    (
        StatusCode::ACCEPTED,
        Json(json!({
            "status": "enrichment_run_started",
            "premium": is_premium,
            "scope": if is_premium { "full_library" } else { "limited" },
            // Portée par répertoire (#1660) — null quand la passe est complète.
            "directory": scope.as_ref().map(|s| s.dir.clone()),
            "directory_tracks": scope.as_ref().map(|s| s.track_count),
            "directory_albums": scope.as_ref().map(|s| s.album_ids.len()),
            "directory_artists": scope.as_ref().map(|s| s.artist_ids.len()),
        })),
    )
}

// ---------------------------------------------------------------------------
// POST /system/cleanup — existing cleanup (unchanged)
// ---------------------------------------------------------------------------

pub(super) async fn cleanup(
    _admin: crate::auth::RequireAdmin,
    State(state): State<AppState>,
) -> Result<Json<Value>, AppError> {
    let album_repo = AlbumRepo::with_backend(state.backend.clone());
    let artist_repo = ArtistRepo::with_backend(state.backend.clone());

    let merged_albums = merge_duplicate_albums(&state.backend)?;
    let orphan_albums = album_repo.delete_orphans().unwrap_or(0);
    let orphan_artists = artist_repo.cleanup_orphans().unwrap_or(0);
    let tracks = TrackRepo::with_backend(state.backend.clone())
        .deduplicate()
        .unwrap_or(0);

    let orphan_artwork = cleanup_orphan_artwork(&state.backend)?;

    let db_optimized = if state.backend.engine() == tune_core::db::engine::Engine::Sqlite {
        state
            .backend
            .execute_batch("PRAGMA optimize; ANALYZE;")
            .is_ok()
    } else {
        state.backend.execute_batch("ANALYZE;").is_ok()
    };

    Ok(Json(json!({
        "duplicate_albums_merged": merged_albums,
        "orphan_albums_deleted": orphan_albums,
        "orphan_artists_deleted": orphan_artists,
        "duplicate_tracks_removed": tracks,
        "orphan_artwork_deleted": orphan_artwork,
        "db_optimized": db_optimized,
    })))
}

fn merge_duplicate_albums(
    db: &std::sync::Arc<dyn tune_core::db::backend::DbBackend>,
) -> Result<i64, AppError> {
    // Group by (LOWER(title), artist_id) so that albums with the same title
    // but different artists are NOT merged (e.g. "One by One" by Grey Reverend
    // vs "One by One" by Robert Francis).
    let dupe_rows = db.query_many(
        "SELECT LOWER(title), GROUP_CONCAT(id) FROM albums WHERE source = 'local' GROUP BY LOWER(title), artist_id HAVING COUNT(id) > 1",
        &[],
    ).ou_defaut_journalise();
    let dupes: Vec<(String, String)> = dupe_rows
        .iter()
        .map(|r| {
            (
                r[0].as_string().unwrap_or_default(),
                r[1].as_string().unwrap_or_default(),
            )
        })
        .collect();

    let mut deleted = 0i64;
    for (_title, ids_str) in &dupes {
        let ids: Vec<i64> = ids_str.split(',').filter_map(|s| s.parse().ok()).collect();
        if ids.len() < 2 {
            continue;
        }
        let mut best_id = ids[0];
        let mut best_count = 0i64;
        for &aid in &ids {
            let cnt = db
                .query_one("SELECT COUNT(id) FROM tracks WHERE album_id = ?", &[&aid])
                .ok()
                .flatten()
                .and_then(|r| r[0].as_i64())
                .unwrap_or(0);
            if cnt > best_count {
                best_count = cnt;
                best_id = aid;
            }
        }
        for &aid in &ids {
            if aid != best_id {
                db.execute(
                    "UPDATE tracks SET album_id = ? WHERE album_id = ?",
                    &[&best_id, &aid],
                )
                .ok();
                db.execute("DELETE FROM albums WHERE id = ?", &[&aid]).ok();
                deleted += 1;
            }
        }
    }
    db.execute_batch(
        "UPDATE albums SET track_count = (SELECT COUNT(t.id) FROM tracks t WHERE t.album_id = albums.id)"
    ).ok();
    Ok(deleted)
}

fn cleanup_orphan_artwork(
    db: &std::sync::Arc<dyn tune_core::db::backend::DbBackend>,
) -> Result<i64, AppError> {
    let cache_dir = crate::routes::library::artwork_cache_dir();
    if !cache_dir.exists() {
        return Ok(0);
    }

    let rows = db
        .query_many(
            "SELECT cover_path FROM albums WHERE cover_path IS NOT NULL \
         UNION SELECT image_path FROM artists WHERE image_path IS NOT NULL",
            &[],
        )
        .ou_defaut_journalise();
    let mut referenced: std::collections::HashSet<String> = std::collections::HashSet::new();
    for r in &rows {
        if let Some(path) = r[0].as_string() {
            referenced.insert(path);
        }
    }

    // Walk artwork cache and delete files whose stem (hash) isn't referenced
    let mut deleted = 0i64;
    if let Ok(entries) = std::fs::read_dir(&cache_dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_file() {
                let stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or("");
                if !stem.is_empty() && !referenced.contains(stem) {
                    if std::fs::remove_file(&path).is_ok() {
                        deleted += 1;
                    }
                }
            }
        }
    }

    if deleted > 0 {
        tracing::info!(deleted, "orphan_artwork_cleaned");
    }
    Ok(deleted)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// Le motif que ce correctif ferme : `/system/enrichment/run` lance quatre
    /// passes en parallèle et le client attend `library.enrich.completed`.
    /// L'annonce doit tomber APRÈS la dernière passe — une annonce anticipée
    /// ferait relire des compteurs encore inchangés, c'est-à-dire exactement le
    /// symptôme de #2259 sous une autre forme.
    #[tokio::test]
    async fn la_fin_de_passe_est_annoncee_apres_la_derniere_tache() {
        let bus = Arc::new(tune_core::event_bus::EventBus::new());
        let mut rx = bus.subscribe();
        let faites = Arc::new(AtomicUsize::new(0));

        // Des durées volontairement désordonnées : la plus longue n'est pas la
        // dernière lancée, donc un `await` oublié se voit.
        let taches = [40u64, 10, 60, 20]
            .into_iter()
            .map(|ms| {
                let faites = faites.clone();
                tokio::spawn(async move {
                    tokio::time::sleep(std::time::Duration::from_millis(ms)).await;
                    faites.fetch_add(1, Ordering::SeqCst);
                })
            })
            .collect();

        // Le compteur se relève À L'INSTANT de la réception, jamais après coup.
        // Lu après le retour de la fonction il vaudrait 4 même si l'annonce
        // était partie en premier : c'est exactement ce qu'a montré la
        // contre-épreuve, où déplacer l'émission avant la jonction laissait le
        // test au vert. Un guetteur concurrent est le seul montage qui date
        // l'annonce par rapport aux passes.
        let temoin = faites.clone();
        let guetteur = tokio::spawn(async move {
            let ev = rx
                .recv()
                .await
                .expect("aucun evenement de fin de passe emis");
            let a_l_annonce = temoin.load(Ordering::SeqCst);
            let encore = rx.try_recv().is_ok();
            (ev.event_type, a_l_annonce, encore)
        });

        annoncer_fin_de_passe(taches, bus.clone(), json!({ "directory": null })).await;

        let (nom, faites_a_l_annonce, encore) = guetteur.await.unwrap();
        assert_eq!(
            nom, "library.enrich.completed",
            "nom attendu par MetadataView.svelte et SettingsView.svelte"
        );
        assert_eq!(
            faites_a_l_annonce, 4,
            "annonce emise avant la fin des quatre passes"
        );
        assert!(!encore, "la fin de passe s'annonce une seule fois");
    }

    /// Une passe qui panique ne doit pas laisser l'écran figé pour toujours :
    /// les autres ont travaillé, la bibliothèque a bougé.
    #[tokio::test]
    async fn une_passe_qui_panique_ne_retient_pas_l_annonce() {
        let bus = Arc::new(tune_core::event_bus::EventBus::new());
        let mut rx = bus.subscribe();

        let taches = vec![
            tokio::spawn(async { panic!("passe en echec") }),
            tokio::spawn(async {}),
        ];

        annoncer_fin_de_passe(taches, bus.clone(), json!({ "directory": null })).await;

        let ev = rx.try_recv().expect("une panique a supprime l'annonce");
        assert_eq!(ev.event_type, "library.enrich.completed");
    }

    /// La portée par répertoire (#1660) voyage dans la charge : un client qui a
    /// demandé un dossier doit pouvoir ne rafraîchir que lui.
    #[tokio::test]
    async fn la_portee_voyage_dans_la_charge() {
        let bus = Arc::new(tune_core::event_bus::EventBus::new());
        let mut rx = bus.subscribe();

        annoncer_fin_de_passe(vec![], bus.clone(), json!({ "directory": "/music/Jazz" })).await;

        let ev = rx.try_recv().unwrap();
        assert_eq!(ev.data["directory"].as_str(), Some("/music/Jazz"));
    }
}
