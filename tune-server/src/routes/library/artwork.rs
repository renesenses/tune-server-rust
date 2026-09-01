use axum::Json;
use axum::extract::{Multipart, Path, Query, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode};
use axum::response::IntoResponse;
use serde::Deserialize;
use serde_json::{Value, json};
use tune_http_types::panne_sql::OuDefautJournalise;

use crate::state::AppState;
use tune_core::db::album_repo::AlbumRepo;
use tune_core::db::track_repo::TrackRepo;

use super::artwork_cache_dir;

fn is_hex_hash(s: &str) -> bool {
    (s.len() == 32 || s.len() == 64) && s.chars().all(|c| c.is_ascii_hexdigit())
}

#[derive(Deserialize)]
pub(super) struct ProxyQuery {
    url: String,
}

/// Le `?size=` que le client envoie sur chaque vignette.
///
/// Le champ est une `String` et non un `u32` **volontairement** : avec un
/// `Option<u32>`, `?size=abc` ferait rendre un 400 par l'extracteur, alors
/// qu'aujourd'hui cette même requête rend l'image. Une taille illisible doit
/// dégrader vers l'original, jamais transformer une pochette en erreur.
#[derive(Deserialize)]
pub(super) struct ArtworkQuery {
    size: Option<String>,
}

/// Sert une pochette, éventuellement redimensionnée.
///
/// Le client construit `?size=200` sur toutes les grilles et `?size=80` sur le
/// tableau de bord (`tune-web-client/src/lib/api.ts:2593`) depuis toujours ;
/// la route n'avait aucun extracteur `Query` et rendait le fichier d'origine.
/// Une vignette de 80 px téléchargeait la pochette entière (#2996).
pub(super) async fn serve_artwork(
    Path(hash): Path<String>,
    Query(query): Query<ArtworkQuery>,
) -> impl IntoResponse {
    let taille = query.size.as_deref().and_then(|s| s.parse::<u32>().ok());
    serve_artwork_from(&artwork_cache_dir(), &hash, taille).await
}

/// Sert une entrée du cache de pochettes, répertoire donné.
///
/// Séparée de [`serve_artwork`] pour être éprouvée sans variable
/// d'environnement : `artwork_cache_dir()` lit `TUNE_ARTWORK_DIR`, qui est
/// commun à tout le processus et donc inutilisable depuis des tests parallèles.
///
/// La liste des extensions cherchées n'est plus écrite ici : c'est
/// [`tune_core::library::artwork::CACHE_EXTENSIONS`], la même que celle sous
/// laquelle l'écriture dépose ses fichiers. Deux listes séparées, c'était la
/// porte ouverte à un condensat annoncé en base et introuvable ici (#2567).
async fn serve_artwork_from(
    cache_dir: &std::path::Path,
    hash: &str,
    size: Option<u32>,
) -> axum::response::Response {
    if let Some((path, mime)) = tune_core::library::artwork::find_cached(cache_dir, hash)
        && let Ok(data) = tokio::fs::read(&path).await
    {
        // Sans `?size=`, rien ne change : mêmes octets, même type MIME, même
        // ETag qu'avant. C'est le chemin que prennent l'écran Lecture en cours
        // et toutes les vues qui n'envoient pas de taille.
        if let Some(bucket) = size.and_then(tune_core::library::artwork::thumb_bucket)
            && let Some(vignette) = vignette(cache_dir, hash, bucket, &data).await
        {
            return reponse_pochette(vignette, "image/jpeg", &format!("{hash}-w{bucket}"));
        }
        return reponse_pochette(data, mime, hash);
    }
    journaliser_absence(cache_dir, hash);
    StatusCode::NOT_FOUND.into_response()
}

fn reponse_pochette(data: Vec<u8>, mime: &str, etag: &str) -> axum::response::Response {
    let mut headers = HeaderMap::new();
    headers.insert(
        "Content-Type",
        HeaderValue::from_str(mime).unwrap_or(HeaderValue::from_static("image/jpeg")),
    );
    headers.insert(
        "Cache-Control",
        HeaderValue::from_static("public, max-age=31536000, immutable"),
    );
    // L'ETag d'une vignette porte sa case (`{condensat}-w200`). Deux tailles de
    // la même pochette sont deux corps différents : leur donner le même ETag
    // sous `immutable` laisserait un cache intermédiaire servir la vignette de
    // 80 px là où la grille demande 200. L'original, lui, garde le condensat
    // seul — ne pas invalider d'un coup ce qui est déjà dans les navigateurs.
    headers.insert(
        "ETag",
        HeaderValue::from_str(&format!("\"{etag}\""))
            .unwrap_or(HeaderValue::from_static("\"artwork\"")),
    );
    (StatusCode::OK, headers, data).into_response()
}

/// Vignette d'une pochette dans une case, depuis le cache ou fabriquée.
///
/// `None` = servir l'original (format non décodable, image déjà plus petite
/// que la case). Le redimensionnement est du calcul pur : il part sur
/// `spawn_blocking` plutôt que de tenir un fil de l'exécuteur pendant les
/// 6 à 31 ms mesurées.
async fn vignette(
    cache_dir: &std::path::Path,
    hash: &str,
    bucket: u32,
    original: &[u8],
) -> Option<Vec<u8>> {
    let chemin = tune_core::library::artwork::thumb_path(cache_dir, bucket, hash);
    if let Ok(deja) = tokio::fs::read(&chemin).await {
        return Some(deja);
    }
    let octets = original.to_vec();
    let vignette = tokio::task::spawn_blocking(move || {
        tune_core::library::artwork::make_thumbnail(&octets, bucket)
    })
    .await
    .ok()
    .flatten()?;

    let (dir, condensat, copie) = (cache_dir.to_path_buf(), hash.to_string(), vignette.clone());
    let _ = tokio::task::spawn_blocking(move || {
        tune_core::library::artwork::store_thumbnail(&dir, bucket, &condensat, &copie);
    })
    .await;
    Some(vignette)
}

/// Condensats déjà signalés absents, pour ne les signaler qu'une fois.
///
/// `artwork_cache_miss` était l'un des rares journaux dont le volume suit le
/// trafic d'interface et non la taille de la bibliothèque : un 404 n'est pas
/// mis en cache par le navigateur, donc une grille qui affiche 50 pochettes
/// manquantes réécrivait 50 lignes à chaque rendu, indéfiniment après la fin du
/// scan (#2996). Le premier constat par condensat reste un `warn!` — c'est lui
/// qui sert au diagnostic ; les suivants passent en `debug!`.
///
/// Le jeu est **borné** : au-delà de `PLAFOND_ABSENCES` condensats distincts on
/// n'insère plus rien. Une mémoire qui grandit avec la bibliothèque pour tenir
/// un journal serait le même défaut sous une autre forme, et 4096 pochettes
/// distinctes manquantes sont déjà un signal largement suffisant.
const PLAFOND_ABSENCES: usize = 4096;
static ABSENCES_DEJA_SIGNALEES: std::sync::LazyLock<
    std::sync::Mutex<std::collections::HashSet<String>>,
> = std::sync::LazyLock::new(|| std::sync::Mutex::new(std::collections::HashSet::new()));

fn journaliser_absence(cache_dir: &std::path::Path, hash: &str) {
    // Une pochette absente n'existait jusqu'ici que dans la console du testeur :
    // la route ne journalisait ni succès ni échec, et un 404 de pochette était
    // invisible côté serveur (#2567). Le condensat suffit à retrouver l'album
    // (`SELECT id FROM albums WHERE cover_path = …`).
    let premiere_fois = {
        let mut vus = ABSENCES_DEJA_SIGNALEES
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        vus.len() < PLAFOND_ABSENCES && vus.insert(hash.to_string())
    };
    if premiere_fois {
        tracing::warn!(
            hash = %hash,
            cache_dir = %cache_dir.display(),
            "artwork_cache_miss — condensat annoncé sans fichier servable"
        );
    } else {
        tracing::debug!(
            hash = %hash,
            cache_dir = %cache_dir.display(),
            "artwork_cache_miss — déjà signalé"
        );
    }
}

pub(super) async fn album_artwork(
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> impl IntoResponse {
    let repo = AlbumRepo::with_backend(state.backend.clone());
    let album = match repo.get(id) {
        Ok(Some(a)) => a,
        _ => return StatusCode::NOT_FOUND.into_response(),
    };

    if let Some(ref cover_path) = album.cover_path {
        if cover_path.starts_with("http") {
            return axum::response::Redirect::temporary(cover_path).into_response();
        }
        let hash = if is_hex_hash(cover_path) {
            cover_path.to_string()
        } else {
            tune_core::library::artwork::artwork_hash(cover_path)
        };
        return axum::response::Redirect::temporary(&format!("/api/v1/library/artwork/{hash}"))
            .into_response();
    }

    let track_repo = TrackRepo::with_backend(state.backend.clone());
    let tracks = track_repo.list_by_album(id).unwrap_or_default();
    if let Some(track) = tracks.first()
        && let Some(ref file_path) = track.file_path
    {
        let cache_dir = artwork_cache_dir();
        if let Some(hash) =
            tune_core::library::artwork::get_or_extract(std::path::Path::new(file_path), &cache_dir)
        {
            return axum::response::Redirect::temporary(&format!("/api/v1/library/artwork/{hash}"))
                .into_response();
        }
    }

    StatusCode::NOT_FOUND.into_response()
}

pub(super) async fn upload_album_artwork(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    mut multipart: Multipart,
) -> impl IntoResponse {
    let album_repo = AlbumRepo::with_backend(state.backend.clone());
    if album_repo.get(id).ok().flatten().is_none() {
        return (
            StatusCode::NOT_FOUND,
            Json(json!({"error": "album not found"})),
        )
            .into_response();
    }

    let mut image_data: Option<Vec<u8>> = None;
    let mut ext = "jpg".to_string();
    while let Ok(Some(field)) = multipart.next_field().await {
        let name = field.name().unwrap_or("").to_string();
        if name == "image" || name == "file" || name == "artwork" {
            if let Some(ct) = field.content_type() {
                if ct.contains("png") {
                    ext = "png".to_string();
                }
            }
            image_data = field.bytes().await.ok().map(|b| b.to_vec());
        }
    }

    let Some(data) = image_data else {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": "no image provided"})),
        )
            .into_response();
    };

    let cache_dir = artwork_cache_dir();
    // Condensat de CONTENU, plus d'identité figée (#1444). Sous
    // `artwork_hash("album-upload-{id}")`, remplacer la pochette gardait la
    // même URL alors que la route sert `immutable, max-age=31536000` : les
    // clients affichaient l'ancienne image pendant un an — et si l'extension
    // changeait (`.png` après un `.jpg`), les deux fichiers coexistaient et
    // `find_cached` servait l'ancien `.jpg` pour toujours. Une image
    // différente obtient désormais forcément une adresse différente, et
    // l'écriture passe par `save_to_cache` (extension canonique, #2567).
    let hash = tune_core::library::artwork::content_hash(&data);
    if tune_core::library::artwork::save_to_cache(&data, &cache_dir, &hash, &ext).is_none() {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": "failed to save image"})),
        )
            .into_response();
    }

    album_repo.force_update_cover_path(id, &hash).ok();

    // Return the updated album
    match album_repo.get(id) {
        Ok(Some(album)) => Json(json!({
            "album": album.to_json(),
            "hash": hash,
            "size": data.len(),
        }))
        .into_response(),
        _ => Json(json!({
            "album_id": id,
            "hash": hash,
            "size": data.len(),
        }))
        .into_response(),
    }
}

pub(super) async fn proxy_artwork(
    State(state): State<AppState>,
    Query(q): Query<ProxyQuery>,
) -> impl IntoResponse {
    match state.http_client.get(&q.url).send().await {
        Ok(resp) if resp.status().is_success() => {
            let content_type = resp
                .headers()
                .get("content-type")
                .and_then(|v| v.to_str().ok())
                .unwrap_or("image/jpeg")
                .to_string();
            match resp.bytes().await {
                Ok(data) => {
                    let mut headers = HeaderMap::new();
                    headers.insert(
                        "Content-Type",
                        HeaderValue::from_str(&content_type)
                            .unwrap_or(HeaderValue::from_static("image/jpeg")),
                    );
                    headers.insert(
                        "Cache-Control",
                        HeaderValue::from_static("public, max-age=86400"),
                    );
                    (StatusCode::OK, headers, data.to_vec()).into_response()
                }
                Err(_) => StatusCode::BAD_GATEWAY.into_response(),
            }
        }
        _ => StatusCode::BAD_GATEWAY.into_response(),
    }
}

pub(super) async fn enrich_album_artwork(
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> impl IntoResponse {
    let repo = AlbumRepo::with_backend(state.backend.clone());
    let album = match repo.get(id) {
        Ok(Some(a)) => a,
        _ => {
            return (
                StatusCode::NOT_FOUND,
                Json(json!({"error": "album not found"})),
            )
                .into_response();
        }
    };

    // Skip if album already has a non-empty cover
    if album.cover_path.as_ref().is_some_and(|p| !p.is_empty()) {
        return Json(json!({"enriched": false, "reason": "album already has cover art"}))
            .into_response();
    }

    // Step 1: Determine MBID — use existing or search MusicBrainz by artist+title
    let mbid = match album
        .musicbrainz_release_id
        .as_deref()
        .filter(|s| !s.is_empty())
    {
        Some(id) => Some(id.to_string()),
        None => {
            let artist = album.artist_name.as_deref().unwrap_or("");
            if !artist.is_empty() && !album.title.is_empty() {
                let found =
                    tune_core::library::artwork::search_musicbrainz_release(artist, &album.title)
                        .await;
                if let Some(ref discovered_mbid) = found {
                    // Store the discovered MBID on the album for future use
                    state.backend.execute(
                        "UPDATE albums SET musicbrainz_release_id = ? WHERE id = ? AND (musicbrainz_release_id IS NULL OR musicbrainz_release_id = '')",
                        &[discovered_mbid as &dyn tune_core::db::backend::ToSqlValue, &id as &dyn tune_core::db::backend::ToSqlValue],
                    ).ok();
                    tracing::info!(
                        album_id = id,
                        mbid = %discovered_mbid,
                        album = %album.title,
                        artist = %artist,
                        "enrich_album_artwork_mbid_discovered"
                    );
                }
                found
            } else {
                None
            }
        }
    };

    let Some(ref mbid_val) = mbid else {
        return Json(json!({
            "enriched": false,
            "reason": "no MusicBrainz release ID and could not find one by artist/title"
        }))
        .into_response();
    };

    // Step 2: Fetch cover from Cover Art Archive
    match tune_core::library::artwork::fetch_cover_art(mbid_val).await {
        Some(data) => {
            let cache_dir = artwork_cache_dir();
            // Adressage par le CONTENU (#1444) : sous `artwork_hash(mbid)`,
            // deux albums partageant un MBID écrivaient au même endroit et un
            // ré-enrichissement réécrivait sous une adresse déjà servie
            // `immutable, max-age=31536000`.
            if let Some(hash) =
                tune_core::library::artwork::cache_fetched_image(&data, &cache_dir, "jpg")
            {
                repo.update_cover_path(id, &hash).ok();
                Json(json!({"enriched": true, "hash": hash, "size": data.len(), "mbid": mbid_val}))
                    .into_response()
            } else {
                Json(json!({"enriched": false, "reason": "failed to save to cache"}))
                    .into_response()
            }
        }
        None => {
            Json(json!({"enriched": false, "reason": "no cover art found on Cover Art Archive"}))
                .into_response()
        }
    }
}

pub(super) async fn batch_enrich_artwork(State(state): State<AppState>) -> impl IntoResponse {
    let cache_dir = artwork_cache_dir();
    let db = state.backend.clone();

    // Check how many albums are missing covers
    let album_repo = AlbumRepo::with_backend(state.backend.clone());
    let missing = album_repo.list_without_cover().unwrap_or_default();

    if missing.is_empty() {
        return Json(json!({
            "status": "skipped",
            "message": "all albums already have cover art",
            "missing": 0,
        }))
        .into_response();
    }

    // Store initial status
    let settings = tune_core::db::settings_repo::SettingsRepo::with_backend(state.backend.clone());
    settings.set("artwork_enrich_status", "running").ok();
    settings
        .set(
            "artwork_enrich_result",
            &json!({"total": missing.len(), "enriched": 0, "status": "running"}).to_string(),
        )
        .ok();

    let task_guard = state.background_tasks.begin(
        "artwork",
        "Récupération des pochettes d'albums…",
        "enrichment",
    );
    tokio::spawn(async move {
        let _task_guard = task_guard; // ends the task when this future completes
        tune_core::library::artwork::batch_enrich_artwork(db, cache_dir).await;
    });

    (
        StatusCode::ACCEPTED,
        Json(json!({
            "status": "accepted",
            "message": "batch artwork enrichment started",
            "albums_to_process": missing.len(),
        })),
    )
        .into_response()
}

pub(super) async fn batch_enrich_artwork_status(State(state): State<AppState>) -> Json<Value> {
    let settings = tune_core::db::settings_repo::SettingsRepo::with_backend(state.backend.clone());
    let result = settings
        .get("artwork_enrich_result")
        .ok()
        .flatten()
        .and_then(|s| serde_json::from_str::<Value>(&s).ok());

    let album_repo = AlbumRepo::with_backend(state.backend.clone());
    let still_missing = album_repo.list_without_cover().unwrap_or_default().len();

    Json(json!({
        "result": result,
        "albums_without_cover": still_missing,
    }))
}

/// Décompte des artistes que la passe d'enrichissement va **réellement**
/// traiter, ventilé par population.
///
/// Les quatre champs reprennent, une pour une, les quatre listes que
/// `batch_enrich_artist_artwork_inner` empile avant de boucler
/// (`tune-core/src/library/artwork.rs`) : c'est la seule façon d'annoncer un
/// total qui corresponde au travail lancé.
pub(super) struct ArtistesSansImage {
    /// MBID connu, aucune image en base (`list_without_image`).
    pub avec_mbid: usize,
    /// MBID connu, `image_path` posé mais le fichier de cache a disparu.
    pub cache_perdu_avec_mbid: usize,
    /// Aucun MBID, aucune image (`list_without_image_no_mbid`).
    pub sans_mbid: usize,
    /// Aucun MBID, `image_path` posé mais le fichier de cache a disparu.
    pub cache_perdu_sans_mbid: usize,
}

impl ArtistesSansImage {
    /// Tout artiste que l'utilisateur voit sans photo.
    ///
    /// Les deux termes « sans MBID » manquaient : `list_without_image` et
    /// `list_with_image_and_mbid` exigent l'une comme l'autre
    /// `musicbrainz_id != ''`. Sur une bibliothèque non étiquetée le total
    /// tombait donc à zéro alors que la passe traite tout le monde (#2184).
    pub fn total(&self) -> usize {
        self.avec_mbid + self.cache_perdu_avec_mbid + self.sans_mbid + self.cache_perdu_sans_mbid
    }

    /// Les images « fantômes » : la base annonce une photo, le fichier a disparu.
    pub fn cache_perdu(&self) -> usize {
        self.cache_perdu_avec_mbid + self.cache_perdu_sans_mbid
    }
}

/// Compte les artistes sans image visible, MBID ou pas.
pub(super) fn compter_artistes_sans_image(
    artist_repo: &tune_core::db::artist_repo::ArtistRepo,
    cache_dir: &std::path::Path,
) -> ArtistesSansImage {
    let perdu = |image_path: &str| {
        !tune_core::library::artwork::cached_artwork_exists(cache_dir, image_path)
    };
    ArtistesSansImage {
        avec_mbid: artist_repo.list_without_image().unwrap_or_default().len(),
        cache_perdu_avec_mbid: artist_repo
            .list_with_image_and_mbid()
            .unwrap_or_default()
            .into_iter()
            .filter(|(_, _, _, image_path)| perdu(image_path))
            .count(),
        sans_mbid: artist_repo
            .list_without_image_no_mbid()
            .unwrap_or_default()
            .len(),
        cache_perdu_sans_mbid: artist_repo
            .list_with_image_no_mbid()
            .unwrap_or_default()
            .into_iter()
            .filter(|(_, _, image_path)| perdu(image_path))
            .count(),
    }
}

/// Libellé affiché à côté du compteur d'avancement, d'après la passe en cours.
///
/// La passe par nom (Discogs / Last.fm) tombait dans le cas par défaut et
/// s'annonçait « MusicBrainz » — la seule des trois qui n'interroge justement
/// pas MusicBrainz. Le nom de la passe vient de `tune-core`, pas d'une chaîne
/// recopiée ici.
pub(super) fn libelle_phase(phase: Option<&str>) -> &'static str {
    match phase {
        Some("images") => "Images",
        Some(p) if p == tune_core::library::artwork::PHASE_PAR_NOM => "Discogs / Last.fm",
        _ => "MusicBrainz",
    }
}

/// Clé du réglage où `tune-core` écrit l'avancement fin de l'enrichissement
/// d'images d'artistes, et identifiant de la tâche de fond correspondante.
const REGLAGE_AVANCEMENT_IMAGES_ARTISTES: &str = "artist_artwork_enrich_result";
const TACHE_IMAGES_ARTISTES: &str = "artist_artwork";

/// Le drapeau nu, écrit à côté du réglage détaillé. Personne ne le lit
/// aujourd'hui, mais il est neutralisé au démarrage comme le réglage détaillé
/// (`startup::DRAPEAUX_AVANCEMENT_ENRICHISSEMENT`) : le laisser mentir en base
/// finirait par trouver un lecteur.
const DRAPEAU_AVANCEMENT_IMAGES_ARTISTES: &str = "artist_artwork_enrich_status";

/// Période de recopie du réglage vers le registre des tâches de fond.
const PERIODE_SONDAGE_AVANCEMENT: std::time::Duration = std::time::Duration::from_secs(3);

/// Interrompt la sonde d'avancement dès sa chute — fin normale du travail,
/// retour anticipé ou panique.
///
/// C'est ce garde qui remplace le plafond de tours : la sonde n'a plus besoin
/// de se limiter d'elle-même puisque plus rien ne peut la laisser tourner seule.
struct SondeEnCours(tokio::task::JoinHandle<()>);

impl Drop for SondeEnCours {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// Recopie l'avancement fin de l'enrichissement d'images d'artistes — écrit par
/// `tune-core` dans le réglage `artist_artwork_enrich_result` — vers le registre
/// des tâches de fond, pour que l'indicateur global affiche « Images 340/1183 »
/// au lieu d'une présence nue.
///
/// S'arrête d'elle-même dès que le réglage annonce autre chose que `running`.
async fn sonder_avancement_images_artistes(
    bg_tasks: crate::background_tasks::BackgroundTasks,
    db: std::sync::Arc<dyn tune_core::db::backend::DbBackend>,
) {
    let settings = tune_core::db::settings_repo::SettingsRepo::with_backend(db);
    loop {
        tokio::time::sleep(PERIODE_SONDAGE_AVANCEMENT).await;
        let Some(raw) = settings
            .get(REGLAGE_AVANCEMENT_IMAGES_ARTISTES)
            .ok()
            .flatten()
        else {
            continue;
        };
        let Ok(v) = serde_json::from_str::<serde_json::Value>(&raw) else {
            continue;
        };
        if v.get("status").and_then(|s| s.as_str()) != Some("running") {
            break;
        }
        let processed = v.get("processed").and_then(|n| n.as_u64()).unwrap_or(0);
        let total = v.get("total").and_then(|n| n.as_u64()).unwrap_or(0);
        let detail = libelle_phase(v.get("phase").and_then(|s| s.as_str()));
        bg_tasks.update_progress(TACHE_IMAGES_ARTISTES, processed, total, detail);
    }
}

/// Exécute `travail` en publiant son avancement pendant toute sa durée.
///
/// Les deux passes d'images d'artistes — celle des manquantes et la reprise
/// forcée — enregistrent la même tâche de fond, mais **seule la première
/// sondait**. La reprise forcée déclarait sa présence puis n'écrivait plus
/// rien : l'indicateur global restait sur « Récupération forcée des images
/// d'artistes… », sans compteur, pendant tout le travail. Or c'est la passe la
/// PLUS longue des deux, puisqu'elle reprend TOUS les artistes et non les seuls
/// artistes sans image ; c'est précisément celle sur laquelle un testeur conclut
/// « il ne se passe rien » (#2073, Fuccaro).
///
/// Le suivi vit donc ici, en un seul endroit, et les deux passes le partagent.
///
/// La sonde ne se plafonne plus à 1200 tours de trois secondes. Ce plafond
/// valait **une heure**, au-delà de laquelle l'avancement gelait alors que la
/// passe continuait : sur une bibliothèque non étiquetée la résolution des MBID
/// coûte à elle seule une seconde par artiste, donc plus d'une heure dès le
/// millier. La sonde s'arrête maintenant sur ce qui la concerne vraiment — la
/// fin du travail, par [`SondeEnCours`], ou un réglage qui n'annonce plus
/// `running`.
async fn sous_suivi_davancement<F>(
    bg_tasks: crate::background_tasks::BackgroundTasks,
    db: std::sync::Arc<dyn tune_core::db::backend::DbBackend>,
    travail: F,
) where
    F: std::future::Future<Output = ()>,
{
    let _sonde = SondeEnCours(tokio::spawn(sonder_avancement_images_artistes(
        bg_tasks, db,
    )));
    travail.await;
}

/// Garantit que la passe d'images d'artistes annonce sa fin, quoi qu'il arrive.
///
/// Le `phase: "done"` de fin est posé APRÈS la boucle, dans
/// `tune_core::library::artwork::batch_enrich_artist_artwork_inner`. Deux
/// sorties le sautent : le retour anticipé quand la liste d'artistes est
/// illisible (`batch_artist_artwork_list_failed`), et une panique de la tâche.
/// Le réglage affirme alors `running` pour toujours.
///
/// Pour la reprise FORCÉE, ce `phase === 'done'` est le **seul** signal de fin
/// qui existe : `SettingsView.svelte` écarte volontairement
/// `artists_without_image === 0` comme condition d'arrêt de cette passe — elle
/// reprend précisément des artistes qui « ont » déjà une image, donc ce nombre
/// vaut zéro d'un bout à l'autre. Sans fin annoncée, son bandeau reste ouvert
/// jusqu'au plafond de sécurité de trente minutes, puis annonce une réussite
/// qui n'a pas eu lieu (#2073).
///
/// Même geste que [`FinDeReprise`] pour les pochettes, et **même réécriture**
/// que `startup::avancement_interrompu` au démarrage : une seule règle, deux
/// déclencheurs — la tâche s'arrête, ou le processus redémarre. Les compteurs
/// sont conservés : « interrompu à 340 / 1183 » se comprend.
///
/// Sur le chemin normal, c'est un non-geste : la boucle a déjà écrit
/// `status: "done"`, et `avancement_interrompu` ne touche que du `running`.
struct FinDePasseArtistes {
    settings: tune_core::db::settings_repo::SettingsRepo,
}

impl FinDePasseArtistes {
    fn nouvelle(db: std::sync::Arc<dyn tune_core::db::backend::DbBackend>) -> Self {
        Self {
            settings: tune_core::db::settings_repo::SettingsRepo::with_backend(db),
        }
    }
}

impl Drop for FinDePasseArtistes {
    fn drop(&mut self) {
        rendre_la_main_si_la_passe_n_a_pas_annonce_sa_fin(&self.settings);
    }
}

/// La réécriture portée par [`FinDePasseArtistes`], hors du `Drop` pour être
/// éprouvable directement.
fn rendre_la_main_si_la_passe_n_a_pas_annonce_sa_fin(
    settings: &tune_core::db::settings_repo::SettingsRepo,
) {
    if let Ok(Some(brut)) = settings.get(REGLAGE_AVANCEMENT_IMAGES_ARTISTES)
        && let Some((neuf, traite, total)) = crate::startup::avancement_interrompu(&brut)
    {
        match settings.set(REGLAGE_AVANCEMENT_IMAGES_ARTISTES, &neuf) {
            Ok(()) => tracing::info!(
                traite,
                total,
                "images_artistes_passe_marquee_interrompue — la tâche s'est arrêtée sans écrire sa fin ; le bandeau du client est rendu à l'utilisateur"
            ),
            Err(e) => tracing::warn!(error = %e, "images_artistes_fin_interrompue_echec"),
        }
    }

    // Le drapeau nu suit le même sort qu'au démarrage : `running` sans passe
    // vivante est un mensonge en base.
    if let Ok(Some(v)) = settings.get(DRAPEAU_AVANCEMENT_IMAGES_ARTISTES)
        && v == "running"
        && let Err(e) = settings.set(DRAPEAU_AVANCEMENT_IMAGES_ARTISTES, "interrupted")
    {
        tracing::warn!(error = %e, "images_artistes_drapeau_interrompu_echec");
    }
}

pub(super) async fn batch_enrich_artist_artwork(
    State(state): State<AppState>,
) -> impl IntoResponse {
    let cache_dir = artwork_cache_dir();
    let db = state.backend.clone();

    // Count artists missing MBIDs (Phase 1 candidates)
    let artist_repo = tune_core::db::artist_repo::ArtistRepo::with_backend(state.backend.clone());
    let without_mbid = artist_repo.list_without_mbid().unwrap_or_default().len();

    // Le décompte annoncé doit couvrir la MÊME population que celle que les
    // phases vont traiter — y compris les artistes sans MBID, que toutes les
    // listes « with_mbid » excluent par construction.
    let sans_image = compter_artistes_sans_image(&artist_repo, &cache_dir);
    let broken_cache = sans_image.cache_perdu();

    if sans_image.total() == 0 && without_mbid == 0 {
        return Json(json!({
            "status": "skipped",
            "message": "all artists already have MBID and images",
            "missing": 0,
        }))
        .into_response();
    }

    // Store initial status
    let settings = tune_core::db::settings_repo::SettingsRepo::with_backend(state.backend.clone());
    settings
        .set(DRAPEAU_AVANCEMENT_IMAGES_ARTISTES, "running")
        .ok();
    settings
        .set(
            REGLAGE_AVANCEMENT_IMAGES_ARTISTES,
            &json!({"total": sans_image.total(), "enriched": 0, "without_mbid": without_mbid, "status": "running"}).to_string(),
        )
        .ok();

    let task_guard = state.background_tasks.begin(
        TACHE_IMAGES_ARTISTES,
        "Récupération des images d'artistes…",
        "enrichment",
    );
    let bg_tasks = state.background_tasks.clone();
    let poll_db = state.backend.clone();
    tokio::spawn(async move {
        let _task_guard = task_guard; // ends the task when this future completes

        // La fin doit être annoncée quoi qu'il arrive : le `phase: "done"` de
        // `tune-core` est posé APRÈS la boucle et deux sorties le sautent.
        let _fin = FinDePasseArtistes::nouvelle(poll_db.clone());

        sous_suivi_davancement(bg_tasks, poll_db, async move {
            // Phase 1: Match artists without MBID by searching MusicBrainz
            let matched = tune_core::metadata::matcher::batch_match_artist_mbids(db.clone()).await;
            tracing::info!(matched, "batch_artist_mbid_phase_complete");

            // Phase 2: Fetch images for all artists with MBID but no image
            tune_core::library::artwork::batch_enrich_artist_artwork(db, cache_dir).await;
        })
        .await;
    });

    (
        StatusCode::ACCEPTED,
        Json(json!({
            "status": "accepted",
            "message": "batch artist enrichment started (Phase 1: MBID matching, Phase 2: image fetch)",
            "artists_without_mbid": without_mbid,
            // Les images fantômes COMPTENT. Le travail vient d'être lancé pour
            // elles — `broken_cache` est justement ce qui empêche le « skipped »
            // ci-dessus — mais la réponse ne les annonçait pas, et l'interface,
            // lisant 0, affichait « Tous les artistes ont déjà une image » puis
            // cessait de suivre l'avancement. L'enrichissement tournait en fond
            // sans que rien ne le dise (Fabien, 11/08/2026 : bibliothèque
            // rescannée, aucune vignette d'artiste, ce message à l'écran).
            //
            // Du point de vue de l'utilisateur, un artiste dont le fichier a
            // disparu n'a pas d'image. C'est ce total-là qui doit être annoncé.
            //
            // Et il en va de même des artistes SANS MBID : `list_without_image`
            // comme `list_with_image_and_mbid` exigent toutes deux
            // `musicbrainz_id != ''`. Sur une bibliothèque non taguée — le cas
            // courant, ~8 % d'identification MusicBrainz — ces deux termes
            // valent zéro alors que la passe travaille des centaines
            // d'artistes en phase 3. Le client lit `artists_without_image === 0`,
            // annonce « Tous les artistes ont déjà une image » et cesse de
            // sonder : deux secondes, aucune image (Bruno Lescarret, #2184,
            // v0.9.44, 738 artistes Windows sans étiquette MusicBrainz).
            "artists_without_image": sans_image.total(),
            // Détaillé à part pour que l'interface puisse expliquer la
            // différence entre « jamais eu d'image » et « image perdue ».
            "artists_with_broken_image": broken_cache,
        })),
    )
        .into_response()
}

/// Force re-fetch of artist images for EVERY artist with an MBID, ignoring the
/// "already has an image" guard. For libraries where image_path is set to
/// stale/broken entries that never render, so the normal pass skips them.
pub(super) async fn force_refetch_artist_artwork(
    State(state): State<AppState>,
) -> impl IntoResponse {
    let cache_dir = artwork_cache_dir();
    let db = state.backend.clone();

    let artist_repo = tune_core::db::artist_repo::ArtistRepo::with_backend(state.backend.clone());
    let total_artists = artist_repo
        .list_all_id_name_mbid()
        .unwrap_or_default()
        .len();

    let settings = tune_core::db::settings_repo::SettingsRepo::with_backend(state.backend.clone());
    settings
        .set(DRAPEAU_AVANCEMENT_IMAGES_ARTISTES, "running")
        .ok();
    settings
        .set(
            REGLAGE_AVANCEMENT_IMAGES_ARTISTES,
            &json!({"total": total_artists, "enriched": 0, "status": "running", "force": true})
                .to_string(),
        )
        .ok();

    let task_guard = state.background_tasks.begin(
        TACHE_IMAGES_ARTISTES,
        "Récupération forcée des images d'artistes…",
        "enrichment",
    );
    let bg_tasks = state.background_tasks.clone();
    let poll_db = state.backend.clone();
    tokio::spawn(async move {
        let _task_guard = task_guard; // ends the task when this future completes

        // La reprise forcée n'a AUCUN autre signal de fin que celui-ci : le
        // client écarte volontairement `artists_without_image === 0` comme
        // condition d'arrêt pour cette passe. Voir [`FinDePasseArtistes`].
        let _fin = FinDePasseArtistes::nouvelle(poll_db.clone());

        // Même suivi que la passe des manquantes : sans lui, la reprise forcée
        // ne publiait QUE sa présence (#2073).
        sous_suivi_davancement(bg_tasks, poll_db, async move {
            // Phase 1: ensure MBIDs are matched, then force re-fetch everyone.
            let matched = tune_core::metadata::matcher::batch_match_artist_mbids(db.clone()).await;
            tracing::info!(matched, "force_artist_mbid_phase_complete");
            tune_core::library::artwork::batch_refetch_artist_artwork(db, cache_dir).await;
        })
        .await;
    });

    (
        StatusCode::ACCEPTED,
        Json(json!({
            "status": "accepted",
            "message": "forced artist artwork re-fetch started (all artists)",
            "artists": total_artists,
        })),
    )
        .into_response()
}

/// Le suivi d'avancement partagé par les deux passes d'images d'artistes.
///
/// Aucun de ces essais ne touche à Discogs, Last.fm, MusicBrainz ni au dépôt
/// communautaire : le « travail » est fourni par l'essai lui-même et se contente
/// d'écrire dans le réglage, exactement comme `tune-core` le fait.
#[cfg(test)]
mod tests_suivi_avancement_images_artistes {
    use super::*;
    use std::sync::{Arc, Mutex};
    use std::time::Duration;
    use tune_core::db::backend::DbBackend;
    use tune_core::db::settings_repo::SettingsRepo;
    use tune_core::event_bus::EventBus;

    use crate::background_tasks::BackgroundTasks;

    fn base_memoire() -> Arc<dyn DbBackend> {
        let db = tune_core::db::sqlite::SqliteDb::open_in_memory().unwrap();
        db.init_schema().unwrap();
        tune_core::db::migrations::run_migrations(&db).unwrap();
        Arc::new(db)
    }

    fn registre() -> BackgroundTasks {
        BackgroundTasks::new(Arc::new(EventBus::new()))
    }

    /// Ce que l'indicateur global affiche : `None` quand la tâche n'annonce que
    /// sa présence, sans compteur.
    fn avancement_affiche(taches: &BackgroundTasks) -> Option<(u64, u64, String)> {
        taches
            .snapshot()
            .into_iter()
            .find(|t| t.id == TACHE_IMAGES_ARTISTES)
            .and_then(|t| t.progress)
            .map(|p| (p.processed, p.total, p.detail))
    }

    /// Ce qu'écrit `tune-core` au fil de la passe des images.
    fn ecrire_avancement(settings: &SettingsRepo, traites: u64, total: u64) {
        settings
            .set(
                REGLAGE_AVANCEMENT_IMAGES_ARTISTES,
                &json!({
                    "status": "running",
                    "phase": "images",
                    "processed": traites,
                    "total": total,
                })
                .to_string(),
            )
            .unwrap();
    }

    /// Le défaut de #2073 : la reprise forcée enregistrait sa tâche et n'y
    /// attachait plus jamais le moindre compteur. L'indicateur global restait
    /// sur son libellé seul pendant toute la passe — la plus longue des deux,
    /// puisqu'elle reprend TOUS les artistes.
    ///
    /// L'essai observe la suite exacte de ce qu'un écran aurait vue.
    #[tokio::test(start_paused = true)]
    async fn la_passe_forcee_publie_son_avancement_pendant_le_travail() {
        let db = base_memoire();
        let taches = registre();
        let _tache = taches.begin(
            TACHE_IMAGES_ARTISTES,
            "Récupération forcée des images d'artistes…",
            "enrichment",
        );

        // Au départ : présence seule, aucun compteur. C'est l'état où la
        // reprise forcée restait bloquée du début à la fin.
        assert_eq!(
            avancement_affiche(&taches),
            None,
            "une tâche fraîchement enregistrée n'annonce que sa présence"
        );

        let settings = SettingsRepo::with_backend(db.clone());
        let vues = Arc::new(Mutex::new(Vec::new()));
        let observateur = taches.clone();
        let journal = vues.clone();

        sous_suivi_davancement(taches.clone(), db.clone(), async move {
            for traites in [5u64, 10, 15] {
                ecrire_avancement(&settings, traites, 15);
                // Plus d'une période de sondage : la sonde a le temps de lire.
                tokio::time::sleep(Duration::from_secs(10)).await;
                journal
                    .lock()
                    .unwrap()
                    .push(avancement_affiche(&observateur));
            }
        })
        .await;

        assert_eq!(
            *vues.lock().unwrap(),
            vec![
                Some((5, 15, "Images".to_string())),
                Some((10, 15, "Images".to_string())),
                Some((15, 15, "Images".to_string())),
            ],
            "l'indicateur doit suivre le travail, pas rester sur la présence"
        );
    }

    /// La sonde se plafonnait à 1200 tours de trois secondes, soit une heure :
    /// au-delà, l'avancement gelait alors que la passe continuait. Une reprise
    /// forcée sur une bibliothèque non étiquetée dépasse l'heure dès le millier
    /// d'artistes — une seconde chacun rien que pour résoudre les MBID.
    #[tokio::test(start_paused = true)]
    async fn le_suivi_ne_gele_plus_apres_une_heure() {
        let db = base_memoire();
        let taches = registre();
        let _tache = taches.begin(TACHE_IMAGES_ARTISTES, "Reprise forcée…", "enrichment");

        let settings = SettingsRepo::with_backend(db.clone());
        let observateur = taches.clone();
        let apres = Arc::new(Mutex::new(None));
        let journal = apres.clone();

        sous_suivi_davancement(taches.clone(), db.clone(), async move {
            ecrire_avancement(&settings, 1, 1000);
            // Au-delà des 1200 tours de 3 s de l'ancien plafond.
            tokio::time::sleep(Duration::from_secs(4000)).await;
            ecrire_avancement(&settings, 900, 1000);
            tokio::time::sleep(Duration::from_secs(10)).await;
            *journal.lock().unwrap() = avancement_affiche(&observateur);
        })
        .await;

        assert_eq!(
            *apres.lock().unwrap(),
            Some((900, 1000, "Images".to_string())),
            "après une heure de travail l'avancement doit encore suivre, pas rester figé sur 1/1000"
        );
    }

    /// Ce qui rend le retrait du plafond sans danger : la sonde s'arrête d'un
    /// réglage qui n'annonce plus `running`, en plus de l'interruption à la fin
    /// du travail. Sans cette sortie, une boucle sans plafond survivrait à la
    /// passe qu'elle observe.
    #[tokio::test(start_paused = true)]
    async fn la_sonde_s_arrete_des_que_la_passe_n_est_plus_en_cours() {
        let db = base_memoire();
        let taches = registre();
        let _tache = taches.begin(TACHE_IMAGES_ARTISTES, "Reprise forcée…", "enrichment");

        let settings = SettingsRepo::with_backend(db.clone());
        let observateur = taches.clone();
        let apres = Arc::new(Mutex::new(None));
        let journal = apres.clone();

        sous_suivi_davancement(taches.clone(), db.clone(), async move {
            ecrire_avancement(&settings, 7, 10);
            tokio::time::sleep(Duration::from_secs(10)).await;

            // La passe se termine : le réglage n'annonce plus `running`.
            settings
                .set(
                    REGLAGE_AVANCEMENT_IMAGES_ARTISTES,
                    &json!({"status": "done", "phase": "done", "total": 10, "enriched": 7})
                        .to_string(),
                )
                .unwrap();
            tokio::time::sleep(Duration::from_secs(10)).await;

            // Une passe SUIVANTE écrit son propre avancement. La sonde de la
            // passe terminée ne doit plus rien en recopier.
            ecrire_avancement(&settings, 999, 999);
            tokio::time::sleep(Duration::from_secs(10)).await;
            *journal.lock().unwrap() = avancement_affiche(&observateur);
        })
        .await;

        assert_eq!(
            *apres.lock().unwrap(),
            Some((7, 10, "Images".to_string())),
            "la sonde arrêtée ne doit plus publier l'avancement d'une autre passe"
        );
    }
}

/// La fin annoncée de la passe d'images d'artistes.
///
/// Aucun de ces essais ne touche à Discogs, Last.fm, MusicBrainz ni au dépôt
/// communautaire : la « passe » est fournie par l'essai lui-même et n'écrit que
/// dans le réglage, exactement comme `tune-core` le fait.
#[cfg(test)]
mod tests_fin_de_passe_images_artistes {
    use super::*;
    use std::sync::Arc;
    use tune_core::db::backend::DbBackend;
    use tune_core::db::settings_repo::SettingsRepo;

    fn base_memoire() -> Arc<dyn DbBackend> {
        let db = tune_core::db::sqlite::SqliteDb::open_in_memory().unwrap();
        db.init_schema().unwrap();
        tune_core::db::migrations::run_migrations(&db).unwrap();
        Arc::new(db)
    }

    fn lire(settings: &SettingsRepo) -> Value {
        serde_json::from_str(
            &settings
                .get(REGLAGE_AVANCEMENT_IMAGES_ARTISTES)
                .unwrap()
                .expect("le réglage d'avancement doit exister"),
        )
        .expect("le réglage doit rester du JSON lisible")
    }

    /// La condition d'arrêt du client pour la reprise FORCÉE, telle que
    /// `SettingsView.svelte` la lit : `phase === 'done'`. Écrite ici pour que
    /// l'essai porte sur ce que l'écran voit, pas sur un champ voisin.
    fn le_client_voit_la_fin(settings: &SettingsRepo) -> bool {
        lire(settings)
            .get("phase")
            .and_then(|p| p.as_str())
            .is_some_and(|p| p == "done")
    }

    /// Ce que la route écrit au lancement de la reprise forcée : ni `phase`, ni
    /// `processed` — seulement le total annoncé et `running`.
    fn passe_forcee_lancee(settings: &SettingsRepo, total: u64) {
        settings
            .set(DRAPEAU_AVANCEMENT_IMAGES_ARTISTES, "running")
            .unwrap();
        settings
            .set(
                REGLAGE_AVANCEMENT_IMAGES_ARTISTES,
                &json!({"total": total, "enriched": 0, "status": "running", "force": true})
                    .to_string(),
            )
            .unwrap();
    }

    /// LE défaut. La reprise forcée s'arrête en cours de route — liste
    /// d'artistes illisible, panique de la tâche — donc `tune-core` n'atteint
    /// jamais le `phase: "done"` posé APRÈS la boucle. Or c'est le seul signal
    /// de fin dont dispose le client pour cette passe : `artists_without_image`
    /// vaut zéro d'un bout à l'autre par construction. Sans fin annoncée, le
    /// bandeau reste ouvert trente minutes puis annonce une réussite qui n'a pas
    /// eu lieu (#2073).
    #[tokio::test]
    async fn la_fin_est_annoncee_meme_si_la_passe_forcee_s_arrete_en_cours_de_route() {
        let db = base_memoire();
        let settings = SettingsRepo::with_backend(db.clone());
        passe_forcee_lancee(&settings, 1183);

        {
            let _fin = FinDePasseArtistes::nouvelle(db.clone());
            // La passe a travaillé, puis s'est arrêtée sans écrire sa fin.
            settings
                .set(
                    REGLAGE_AVANCEMENT_IMAGES_ARTISTES,
                    &json!({
                        "status": "running",
                        "phase": "images",
                        "processed": 340,
                        "total": 1183,
                        "enriched": 12,
                    })
                    .to_string(),
                )
                .unwrap();
            // On sort du bloc sans que la boucle soit allée au bout.
        }

        assert!(
            le_client_voit_la_fin(&settings),
            "sans `phase: \"done\"`, le bandeau de la reprise forcée ne se referme jamais"
        );
        let apres = lire(&settings);
        assert_eq!(
            apres["status"], "interrupted",
            "et il ne ment pas sur l'issue"
        );
        assert_eq!(
            apres["processed"], 340,
            "« interrompu à 340/1183 » se comprend ; un compteur effacé ne dirait plus rien"
        );
        assert_eq!(apres["total"], 1183);
        assert_eq!(
            settings
                .get(DRAPEAU_AVANCEMENT_IMAGES_ARTISTES)
                .unwrap()
                .as_deref(),
            Some("interrupted"),
            "le drapeau nu ne doit pas rester à `running` sans passe vivante"
        );
    }

    /// Le cas le plus traître : la tâche s'arrête AVANT d'avoir traité le
    /// moindre artiste — c'est très exactement le retour anticipé
    /// `batch_artist_artwork_list_failed`. Le réglage est alors resté celui que
    /// la route a écrit : ni `phase`, ni `processed`. Rien ne bougeant jamais,
    /// le client n'a aucun moyen de distinguer cet état d'une passe qui démarre.
    #[tokio::test]
    async fn la_fin_est_annoncee_meme_si_la_passe_forcee_n_a_rien_traite() {
        let db = base_memoire();
        let settings = SettingsRepo::with_backend(db.clone());
        passe_forcee_lancee(&settings, 1183);

        drop(FinDePasseArtistes::nouvelle(db.clone()));

        assert!(
            le_client_voit_la_fin(&settings),
            "une passe morte au premier geste doit rendre la main comme les autres"
        );
        assert_eq!(lire(&settings)["status"], "interrupted");
    }

    /// Contre-épreuve du témoin : sur le chemin NORMAL, `tune-core` a déjà écrit
    /// sa fin. Le garde doit être un non-geste — surtout ne pas repeindre en
    /// « interrompu » une passe qui est allée au bout, ni toucher aux comptes
    /// qu'elle annonce.
    #[tokio::test]
    async fn une_passe_allee_au_bout_n_est_pas_repeinte_en_interrompue() {
        let db = base_memoire();
        let settings = SettingsRepo::with_backend(db.clone());
        passe_forcee_lancee(&settings, 900);

        {
            let _fin = FinDePasseArtistes::nouvelle(db.clone());
            settings
                .set(
                    REGLAGE_AVANCEMENT_IMAGES_ARTISTES,
                    &json!({
                        "status": "done",
                        "phase": "done",
                        "total": 900,
                        "enriched": 137,
                        "failed": 763,
                    })
                    .to_string(),
                )
                .unwrap();
        }

        let apres = lire(&settings);
        assert_eq!(
            apres["status"], "done",
            "la fin normale reste une fin normale"
        );
        assert_eq!(apres["enriched"], 137, "et son bilan n'est pas réécrit");
        assert_eq!(apres["failed"], 763);
    }

    /// Le garde et la sonde vivent dans la même tâche, et l'ORDRE compte : la
    /// sonde doit être arrêtée avant que la fin soit écrite, sinon elle peut
    /// republier un `running` par-dessus. C'est l'assemblage exact des deux
    /// routes qui est éprouvé ici, pas chaque pièce séparément.
    #[tokio::test(start_paused = true)]
    async fn assemblee_comme_dans_la_route_la_passe_interrompue_annonce_sa_fin() {
        use crate::background_tasks::BackgroundTasks;
        use tune_core::event_bus::EventBus;

        let db = base_memoire();
        let settings = SettingsRepo::with_backend(db.clone());
        passe_forcee_lancee(&settings, 50);

        let taches = BackgroundTasks::new(Arc::new(EventBus::new()));
        let garde = taches.begin(TACHE_IMAGES_ARTISTES, "Reprise forcée…", "enrichment");

        // Le corps de `force_refetch_artist_artwork`, à l'identique.
        {
            let _task_guard = garde;
            let _fin = FinDePasseArtistes::nouvelle(db.clone());
            let ecrivain = SettingsRepo::with_backend(db.clone());
            sous_suivi_davancement(taches.clone(), db.clone(), async move {
                ecrivain
                    .set(
                        REGLAGE_AVANCEMENT_IMAGES_ARTISTES,
                        &json!({
                            "status": "running",
                            "phase": "images",
                            "processed": 5,
                            "total": 50,
                        })
                        .to_string(),
                    )
                    .unwrap();
                tokio::time::sleep(std::time::Duration::from_secs(10)).await;
                // La passe s'arrête ici, sans écrire sa fin.
            })
            .await;
        }

        assert!(
            le_client_voit_la_fin(&settings),
            "la sonde s'arrête, PUIS la fin est écrite — et elle reste écrite"
        );
        assert_eq!(lire(&settings)["processed"], 5);
    }
}

/// Le CÂBLAGE, pas seulement les pièces.
///
/// Les essais ci-dessus assemblent le suivi et le garde de fin à la main : ils
/// prouvent que les deux pièces marchent, et resteraient verts si une route
/// cessait de les monter. Vérifié : retirer `FinDePasseArtistes::nouvelle` de
/// `force_refetch_artist_artwork` ne faisait rougir aucun d'eux. Ce garde-là
/// lit le corps des deux routes et exige qu'elles montent les deux pièces —
/// c'est le seul contrôle que la dégradation « la route ne câble plus rien »
/// fasse tomber.
///
/// Une route d'enrichissement ne peut pas être éprouvée en l'appelant : elle
/// part interroger MusicBrainz, Discogs et Last.fm. La source est donc lue.
#[cfg(test)]
mod garde_cablage_des_routes_images_artistes {
    /// Le corps d'une fonction de ce fichier, bornes comprises.
    ///
    /// ⚠️ La découpe est ce qui empêche ce garde de se trouver lui-même :
    /// `include_str!` rend le fichier ENTIER, modules de test compris, et les
    /// motifs cherchés y figurent mot pour mot. Un `contains` sur le fichier
    /// complet rendrait vrai quoi qu'il arrive (#2082).
    fn corps_de(nom: &str) -> &'static str {
        const TOUT: &str = include_str!("artwork.rs");
        let entete = format!("pub(super) async fn {nom}(");
        let debut = TOUT
            .find(&entete)
            .unwrap_or_else(|| panic!("route `{nom}` introuvable : ce garde ne protège plus rien"));
        // L'accolade fermante en colonne zéro : les accolades imbriquées d'un
        // corps de fonction sont toutes indentées.
        let fin = TOUT[debut..]
            .find("\n}\n")
            .unwrap_or_else(|| panic!("fin de `{nom}` introuvable"));
        &TOUT[debut..debut + fin]
    }

    /// Témoin de la découpe. Sans lui, un `corps_de` qui rendrait une tranche
    /// vide ou fausse ferait passer les deux contrôles suivants pour rien.
    #[test]
    fn la_decoupe_rend_bien_le_corps_des_deux_routes() {
        assert!(
            corps_de("force_refetch_artist_artwork")
                .contains("forced artist artwork re-fetch started"),
            "la tranche ne contient pas la réponse de la reprise forcée"
        );
        assert!(
            corps_de("batch_enrich_artist_artwork").contains("batch artist enrichment started"),
            "la tranche ne contient pas la réponse de la passe des manquantes"
        );
        assert!(
            !corps_de("force_refetch_artist_artwork").contains("batch artist enrichment started"),
            "la découpe déborde sur la route voisine : elle ne distingue plus rien"
        );
    }

    /// #2073. La reprise forcée déclarait sa présence dans le registre des
    /// tâches de fond et n'y attachait plus jamais le moindre compteur :
    /// l'indicateur global restait sur « Récupération forcée des images
    /// d'artistes… », sans fraction, pendant toute la passe — la plus longue
    /// des deux, puisqu'elle reprend TOUS les artistes.
    #[test]
    fn les_deux_routes_publient_leur_avancement() {
        for route in [
            "force_refetch_artist_artwork",
            "batch_enrich_artist_artwork",
        ] {
            assert!(
                corps_de(route).contains("sous_suivi_davancement("),
                "`{route}` ne monte plus le suivi d'avancement : l'indicateur global \
                 n'affichera qu'une présence nue, sans compteur"
            );
        }
    }

    /// La fin annoncée. Le `phase: \"done\"` de `tune-core` est posé APRÈS la
    /// boucle ; une passe qui s'arrête avant ne l'écrit jamais, et pour la
    /// reprise forcée c'est le SEUL signal de fin que le client possède.
    #[test]
    fn les_deux_routes_garantissent_une_fin_annoncee() {
        for route in [
            "force_refetch_artist_artwork",
            "batch_enrich_artist_artwork",
        ] {
            assert!(
                corps_de(route).contains("FinDePasseArtistes::nouvelle("),
                "`{route}` n'installe plus le garde de fin : une passe interrompue \
                 laisserait le bandeau du client ouvert pour toujours"
            );
        }
    }
}

pub(super) async fn batch_enrich_artist_artwork_status(
    State(state): State<AppState>,
) -> Json<Value> {
    let settings = tune_core::db::settings_repo::SettingsRepo::with_backend(state.backend.clone());
    let result = settings
        .get(REGLAGE_AVANCEMENT_IMAGES_ARTISTES)
        .ok()
        .flatten()
        .and_then(|s| serde_json::from_str::<Value>(&s).ok());

    let artist_repo = tune_core::db::artist_repo::ArtistRepo::with_backend(state.backend.clone());
    // Même population que la réponse 202 : le client se sert de ce nombre comme
    // condition d'arrêt (`artistImgRemaining === 0` ⇒ « terminé »). Le limiter
    // aux artistes porteurs d'un MBID faisait conclure « terminé » au premier
    // sondage sur toute bibliothèque non taguée.
    let still_missing = compter_artistes_sans_image(&artist_repo, &artwork_cache_dir()).total();

    Json(json!({
        "result": result,
        "artists_without_image": still_missing,
    }))
}

pub(super) async fn rescan_album_artwork(
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> impl IntoResponse {
    let track_repo = TrackRepo::with_backend(state.backend.clone());
    let album_repo = AlbumRepo::with_backend(state.backend.clone());
    let tracks = track_repo.list_by_album(id).unwrap_or_default();
    if tracks.is_empty() {
        return (
            StatusCode::NOT_FOUND,
            Json(json!({"error": "no tracks in album"})),
        )
            .into_response();
    }
    let cache_dir = artwork_cache_dir();
    let mut found_hash: Option<String> = None;
    for track in &tracks {
        if let Some(ref file_path) = track.file_path {
            // `refresh_cover_hash` et non `get_or_extract` : cette route EST le
            // rattrapage manuel. `get_or_extract` sonde d'abord l'entrée
            // héritée, adressée par le CHEMIN de la piste — remplacer
            // `cover.jpg` ne déplace pas ce chemin, donc la sonde rendait
            // l'ancienne image et le `force_update_cover_path` ci-dessous
            // réécrivait la base avec le condensat qu'elle portait déjà. Le
            // bouton ne pouvait rien changer (#3028).
            if let Some(hash) = tune_core::library::artwork::refresh_cover_hash(
                std::path::Path::new(file_path),
                &cache_dir,
            ) {
                found_hash = Some(hash);
                break;
            }
        }
    }
    if let Some(ref hash) = found_hash {
        album_repo.force_update_cover_path(id, hash).ok();
    }
    Json(json!({
        "album_id": id,
        "rescanned_tracks": tracks.len(),
        "artwork_found": found_hash.is_some(),
        "hash": found_hash,
    }))
    .into_response()
}

/// Ce que la reprise des pochettes a fait, a un instant donne.
///
/// UNE seule source pour les DEUX annonces (`library.artwork.progress` et
/// `library.artwork.completed`) : deux `json!` recopies auraient diverge au
/// premier champ ajoute — c'est la lecon du rapport de fin de scan (#2827).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct AvancementPochettes {
    /// Albums deja examines.
    traites: usize,
    /// Albums a examiner. Connu des le depart : la requete les compte tous.
    total: usize,
    /// Albums pour lesquels une pochette a ete posee pendant CETTE passe.
    trouvees: usize,
}

impl AvancementPochettes {
    /// Les trois champs, et RIEN d'autre, que `SettingsView.svelte` rend dans
    /// `settings.coversProgress` — « Covers {current}/{total} ({found}
    /// trouvées) ». Le client fait foi sur le nom et sur la charge utile.
    fn charge(self) -> Value {
        json!({
            "current": self.traites,
            "total": self.total,
            "found": self.trouvees,
        })
    }
}

/// Garantit que `library.artwork.completed` part, quoi qu'il arrive.
///
/// C'est le SEUL evenement qui fasse retomber `artworkScanning` cote client :
/// sans lui, le bouton « Rechercher les pochettes » reste grise jusqu'au
/// rechargement de la page — exactement le symptome de #2870. Un abandon en
/// cours de route (panique dans la tache) doit donc l'annoncer aussi : un
/// compte incomplet vaut mieux qu'un ecran bloque. Meme principe que le
/// superviseur de fin d'enrichissement (#2840), applique par `Drop` puisqu'il
/// n'y a ici qu'une seule tache a surveiller.
struct FinDeReprise {
    bus: std::sync::Arc<tune_core::event_bus::EventBus>,
    avancement: AvancementPochettes,
}

impl Drop for FinDeReprise {
    fn drop(&mut self) {
        self.bus.emit_typed(
            tune_core::event_types::EventType::ArtworkComplete,
            self.avancement.charge(),
        );
    }
}

pub(super) async fn rescan_all_artwork(State(state): State<AppState>) -> impl IntoResponse {
    let cache_dir = artwork_cache_dir();
    let backend = state.backend.clone();
    let event_bus = state.event_bus.clone();

    tokio::spawn(async move {
        let albums: Vec<i64> = backend
            .query_many("SELECT id FROM albums", &[])
            .ou_defaut_journalise()
            .into_iter()
            .filter_map(|row| row.first().and_then(|v| v.as_i64()))
            .collect();

        let track_repo = TrackRepo::with_backend(backend.clone());
        let album_repo = AlbumRepo::with_backend(backend);
        let mut fin = FinDeReprise {
            bus: event_bus.clone(),
            avancement: AvancementPochettes {
                traites: 0,
                total: albums.len(),
                trouvees: 0,
            },
        };
        // Une passe sur une grande bibliotheque lit chaque fichier : emettre par
        // album inonderait le bus. Meme discipline que `library.scan.progress` —
        // la premiere annonce part tout de suite (la barre doit apparaitre), les
        // suivantes au plus toutes les deux secondes.
        let mut cadence = tune_core::cadence::Cadence::avancement();
        if cadence.autorise() {
            event_bus.emit_typed(
                tune_core::event_types::EventType::ArtworkProgress,
                fin.avancement.charge(),
            );
        }
        for album_id in &albums {
            let tracks = track_repo.list_by_album(*album_id).unwrap_or_default();
            for track in &tracks {
                if let Some(ref file_path) = track.file_path {
                    // Même raison qu'au rattrapage par album : sans sauter la
                    // sonde héritée, cette passe réécrit la base avec le
                    // condensat qu'elle portait déjà (#3028).
                    if let Some(hash) = tune_core::library::artwork::refresh_cover_hash(
                        std::path::Path::new(file_path),
                        &cache_dir,
                    ) {
                        album_repo.force_update_cover_path(*album_id, &hash).ok();
                        fin.avancement.trouvees += 1;
                        break;
                    }
                }
            }
            fin.avancement.traites += 1;
            if cadence.autorise() {
                event_bus.emit_typed(
                    tune_core::event_types::EventType::ArtworkProgress,
                    fin.avancement.charge(),
                );
            }
        }
        tracing::info!(
            updated = fin.avancement.trouvees,
            total = fin.avancement.total,
            "rescan_all_artwork done"
        );
        // `fin` tombe ici : `library.artwork.completed` part avec le compte final.
    });

    (
        StatusCode::ACCEPTED,
        Json(json!({"status": "accepted", "message": "artwork rescan started"})),
    )
}

#[cfg(test)]
mod tests_decompte_artistes {
    use super::*;
    use std::sync::Arc;
    use tune_core::db::artist_repo::ArtistRepo;
    use tune_core::db::backend::DbBackend;
    use tune_core::db::models::Artist;

    fn base_memoire() -> Arc<dyn DbBackend> {
        let db = tune_core::db::sqlite::SqliteDb::open_in_memory().unwrap();
        db.init_schema().unwrap();
        tune_core::db::migrations::run_migrations(&db).unwrap();
        Arc::new(db)
    }

    /// Crée un artiste et pose son MBID / son `image_path` s'il y en a un.
    fn artiste(repo: &ArtistRepo, nom: &str, mbid: Option<&str>, image: Option<&str>) {
        let id = repo.create(&Artist::new(nom.into())).unwrap();
        if let Some(m) = mbid {
            repo.update_mbid(id, m).unwrap();
        }
        if let Some(i) = image {
            repo.update_image(id, i, "test").unwrap();
        }
    }

    /// Le cas Bruno Lescarret (#2184) : bibliothèque Windows sans la moindre
    /// étiquette MusicBrainz. Toutes les listes « avec MBID » rendent zéro, et
    /// c'est pourtant tout le travail de la passe.
    #[test]
    fn une_bibliotheque_sans_mbid_ne_compte_pas_zero_artiste_sans_image() {
        let backend = base_memoire();
        let repo = ArtistRepo::with_backend(backend);
        for nom in ["Ange", "Magma", "Gong"] {
            artiste(&repo, nom, None, None);
        }
        let cache = tempfile::tempdir().unwrap();

        let compte = compter_artistes_sans_image(&repo, cache.path());

        assert_eq!(compte.avec_mbid, 0, "aucun artiste n'a de MBID");
        assert_eq!(
            compte.sans_mbid, 3,
            "les trois sont sans MBID et sans image"
        );
        assert_eq!(
            compte.total(),
            3,
            "un artiste sans MBID et sans image est un artiste SANS IMAGE : \
             en annoncer 0 fait afficher « tous les artistes ont déjà une image » \
             et arrête le suivi au bout de deux secondes (#2184)"
        );
    }

    /// Image « fantôme » sans MBID : la base annonce une photo, le fichier de
    /// cache a disparu. La phase 2 la remet en file (`list_with_image_no_mbid`),
    /// donc elle doit être annoncée.
    #[test]
    fn une_image_fantome_sans_mbid_compte_comme_manquante() {
        let backend = base_memoire();
        let repo = ArtistRepo::with_backend(backend);
        artiste(
            &repo,
            "Heldon",
            None,
            Some("cafecafecafecafecafecafecafecafe"),
        );
        let cache = tempfile::tempdir().unwrap();

        let compte = compter_artistes_sans_image(&repo, cache.path());

        assert_eq!(compte.cache_perdu_sans_mbid, 1);
        assert_eq!(compte.total(), 1);
    }

    /// Garde-fou inverse : une photo réellement présente en cache ne doit
    /// jamais être recomptée comme manquante, MBID ou pas.
    #[test]
    fn une_image_presente_en_cache_ne_compte_pas() {
        let backend = base_memoire();
        let repo = ArtistRepo::with_backend(backend);
        let cache = tempfile::tempdir().unwrap();
        let hash = "aaaabbbbccccddddeeeeffff00001111";
        std::fs::write(cache.path().join(format!("{hash}.jpg")), b"jpeg").unwrap();
        artiste(&repo, "Pulsar", None, Some(hash));
        artiste(
            &repo,
            "Shylock",
            Some("11111111-2222-3333-4444-555555555555"),
            Some(hash),
        );

        let compte = compter_artistes_sans_image(&repo, cache.path());

        assert_eq!(compte.total(), 0, "les deux photos sont bien en cache");
        assert_eq!(compte.cache_perdu(), 0);
    }

    /// Les quatre populations à la fois, pour que le total ne puisse pas être
    /// juste « par hasard » sur un seul terme.
    #[test]
    fn le_total_couvre_les_quatre_populations() {
        let backend = base_memoire();
        let repo = ArtistRepo::with_backend(backend);
        let cache = tempfile::tempdir().unwrap();
        let present = "99998888777766665555444433332222";
        std::fs::write(cache.path().join(format!("{present}.jpg")), b"jpeg").unwrap();

        artiste(&repo, "avec mbid, sans image", Some("mbid-a"), None);
        artiste(
            &repo,
            "avec mbid, image fantome",
            Some("mbid-b"),
            Some("00000000000000000000000000000000"),
        );
        artiste(&repo, "sans mbid, sans image", None, None);
        artiste(
            &repo,
            "sans mbid, image fantome",
            None,
            Some("11111111111111111111111111111111"),
        );
        // Témoin : ne doit compter dans aucun terme.
        artiste(&repo, "servi", Some("mbid-c"), Some(present));

        let compte = compter_artistes_sans_image(&repo, cache.path());

        assert_eq!(compte.avec_mbid, 1);
        assert_eq!(compte.cache_perdu_avec_mbid, 1);
        assert_eq!(compte.sans_mbid, 1);
        assert_eq!(compte.cache_perdu_sans_mbid, 1);
        assert_eq!(compte.total(), 4, "quatre artistes sans photo visible");
        assert_eq!(compte.cache_perdu(), 2);
    }
}

// ---------------------------------------------------------------------------
// #2567 — ce que la base annonce, la route doit le servir.
//
// Le client web ne fabrique pas l'identifiant qu'il demande : il recopie
// `cover_path` tel que le serveur le lui a rendu, et le pose derrière
// `/api/v1/library/artwork/`. Un condensat annoncé que cette route ne trouve
// pas, c'est l'image de remplacement à l'écran — et, jusqu'ici, rien du tout
// dans le journal du serveur.
// ---------------------------------------------------------------------------
#[cfg(test)]
mod tests_service_pochette {
    use super::*;

    fn ecrire(dir: &std::path::Path, nom: &str, octets: &[u8]) {
        std::fs::write(dir.join(nom), octets).unwrap();
    }

    /// Les orthographes que l'écriture a réellement produites sur le terrain :
    /// l'extension d'une `cover.jpeg` ou d'une `FOLDER.JPG` était recopiée
    /// telle quelle, et une pochette intégrée BMP était écrite `.bmp`.
    #[tokio::test]
    async fn toute_entree_de_cache_ecrite_est_servie() {
        let cache = tempfile::TempDir::new().unwrap();
        let cas: &[(&str, &str, &str)] = &[
            ("0000000000000000000000000000000a", "jpg", "image/jpeg"),
            ("0000000000000000000000000000000b", "jpeg", "image/jpeg"),
            ("0000000000000000000000000000000c", "JPG", "image/jpeg"),
            ("0000000000000000000000000000000d", "JPEG", "image/jpeg"),
            ("0000000000000000000000000000000e", "png", "image/png"),
            ("0000000000000000000000000000000f", "PNG", "image/png"),
            ("00000000000000000000000000000010", "webp", "image/webp"),
            ("00000000000000000000000000000011", "bmp", "image/bmp"),
        ];
        let mut echecs = Vec::new();
        for (hash, ext, mime) in cas {
            ecrire(cache.path(), &format!("{hash}.{ext}"), b"IMAGE");
            let reponse = serve_artwork_from(cache.path(), hash, None).await;
            let statut = reponse.status();
            let recu = reponse
                .headers()
                .get("Content-Type")
                .and_then(|v| v.to_str().ok())
                .unwrap_or("")
                .to_string();
            if statut != StatusCode::OK || recu != *mime {
                echecs.push(format!(
                    "{ext} → {statut} / {recu:?} (attendu 200 OK / {mime})"
                ));
            }
        }
        assert!(
            echecs.is_empty(),
            "{} orthographe(s) sur {} écrites dans le cache mais non servies (#2567) : {:?}",
            echecs.len(),
            cas.len(),
            echecs
        );
    }

    /// Une entrée adressée par le CONTENU (#1444) — SHA-256, 64 hexdigits —
    /// est servie exactement comme une entrée héritée en 32 : la route ne
    /// distingue pas les deux formes.
    #[tokio::test]
    async fn une_entree_adressee_par_le_contenu_est_servie() {
        let cache = tempfile::TempDir::new().unwrap();
        let hash = tune_core::library::artwork::content_hash(b"NOUVELLE-POCHETTE");
        assert_eq!(hash.len(), 64);
        ecrire(cache.path(), &format!("{hash}.jpg"), b"NOUVELLE-POCHETTE");
        let reponse = serve_artwork_from(cache.path(), &hash, None).await;
        assert_eq!(reponse.status(), StatusCode::OK);
    }

    /// Garde-fou : un condensat sans fichier reste un 404. Servir un octet de
    /// remplacement à sa place ferait croire à une pochette et empêcherait de
    /// jamais la reconstruire.
    #[tokio::test]
    async fn un_condensat_sans_fichier_reste_un_404() {
        let cache = tempfile::TempDir::new().unwrap();
        let reponse =
            serve_artwork_from(cache.path(), "8865c2f2e1a6f89c34ab584ec5b8e158", None).await;
        assert_eq!(reponse.status(), StatusCode::NOT_FOUND);
    }

    /// Garde-fou : l'ETag reste le condensat lui-même. Le changer invaliderait
    /// d'un coup la pochette déjà en cache dans chaque navigateur.
    #[tokio::test]
    async fn l_etag_reste_le_condensat() {
        let cache = tempfile::TempDir::new().unwrap();
        let hash = "8865c2f2e1a6f89c34ab584ec5b8e158";
        ecrire(cache.path(), &format!("{hash}.jpg"), b"IMAGE");
        let reponse = serve_artwork_from(cache.path(), hash, None).await;
        assert_eq!(reponse.status(), StatusCode::OK);
        assert_eq!(
            reponse.headers().get("ETag").unwrap().to_str().unwrap(),
            format!("\"{hash}\"")
        );
        assert_eq!(
            reponse
                .headers()
                .get("Cache-Control")
                .unwrap()
                .to_str()
                .unwrap(),
            "public, max-age=31536000, immutable"
        );
    }
}

/// Contre-épreuve de #2996 : le `?size=` que le client envoie sur chaque
/// vignette était reçu et silencieusement ignoré.
#[cfg(test)]
mod tests_taille_pochette {
    use super::*;

    /// Une vraie pochette JPEG de `cote` pixels, non uniforme pour que
    /// l'encodeur ne la réduise pas à quelques octets.
    fn pochette(cote: u32) -> Vec<u8> {
        let img = image::RgbImage::from_fn(cote, cote, |x, y| {
            image::Rgb([(x % 256) as u8, (y % 256) as u8, ((x * y) % 256) as u8])
        });
        let mut out = std::io::Cursor::new(Vec::new());
        image::codecs::jpeg::JpegEncoder::new_with_quality(&mut out, 90)
            .encode(img.as_raw(), cote, cote, image::ExtendedColorType::Rgb8)
            .unwrap();
        out.into_inner()
    }

    async fn corps(reponse: axum::response::Response) -> Vec<u8> {
        axum::body::to_bytes(reponse.into_body(), usize::MAX)
            .await
            .unwrap()
            .to_vec()
    }

    fn cote_servi(octets: &[u8]) -> u32 {
        image::load_from_memory(octets).unwrap().width()
    }

    fn poser(cache: &std::path::Path, hash: &str, octets: &[u8]) {
        std::fs::write(cache.join(format!("{hash}.jpg")), octets).unwrap();
    }

    /// LE FAIT. Une grille demande `?size=200`, le tableau de bord `?size=80` :
    /// avant le correctif, les deux recevaient la pochette d'origine intacte.
    #[tokio::test]
    async fn le_parametre_size_est_honore() {
        let cache = tempfile::TempDir::new().unwrap();
        let hash = "0000000000000000000000000000aa01";
        let origine = pochette(600);
        poser(cache.path(), hash, &origine);

        let mut echecs = Vec::new();
        for (demande, case) in [
            (80u32, 80u32),
            (200, 200),
            (128, 128),
            (100, 128),
            (300, 400),
        ] {
            let servi = corps(serve_artwork_from(cache.path(), hash, Some(demande)).await).await;
            let cote = cote_servi(&servi);
            if cote != case {
                echecs.push(format!(
                    "?size={demande} → {cote} px servis (case attendue {case} px), {} o contre {} o d'origine",
                    servi.len(),
                    origine.len()
                ));
            }
            if servi.len() >= origine.len() {
                echecs.push(format!(
                    "?size={demande} → {} o, la pochette d'origine en fait {} : rien n'a été économisé",
                    servi.len(),
                    origine.len()
                ));
            }
        }
        assert!(
            echecs.is_empty(),
            "le ?size= envoyé par le client n'est pas honoré (#2996) : {echecs:#?}"
        );
    }

    /// TÉMOIN ANTI-RÉGRESSION. Une requête **sans** `?size=` doit rendre
    /// exactement les octets d'avant, le même type MIME et le même ETag :
    /// c'est le chemin de l'écran Lecture en cours et de toutes les vues qui
    /// n'envoient pas de taille.
    #[tokio::test]
    async fn sans_size_les_octets_sont_inchanges() {
        let cache = tempfile::TempDir::new().unwrap();
        let hash = "0000000000000000000000000000aa02";
        let origine = pochette(600);
        poser(cache.path(), hash, &origine);

        let reponse = serve_artwork_from(cache.path(), hash, None).await;
        assert_eq!(reponse.status(), StatusCode::OK);
        assert_eq!(reponse.headers().get("Content-Type").unwrap(), "image/jpeg");
        assert_eq!(
            reponse.headers().get("ETag").unwrap().to_str().unwrap(),
            format!("\"{hash}\"")
        );
        let servi = corps(reponse).await;
        assert_eq!(
            servi, origine,
            "sans ?size=, les octets servis ne sont plus ceux du cache"
        );
    }

    /// LA CLÉ. Une case est un composant de chemin, pas un morceau de nom de
    /// fichier : on compte les COLLISIONS, pas seulement les pertes (#1444).
    /// Toutes les paires (condensat, case) doivent produire des chemins deux à
    /// deux distincts, et aucun ne doit tomber sur un fichier d'origine.
    #[test]
    fn aucune_collision_de_chemin_entre_deux_cases_ou_deux_condensats() {
        use std::collections::HashMap;
        let cache = std::path::Path::new("/cache");
        let f64x = "f".repeat(64);
        let condensats = [
            "0000000000000000000000000000aa01",
            "0000000000000000000000000000aa02",
            "80000000000000000000000000000000",
            "8865c2f2e1a6f89c34ab584ec5b8e158",
            f64x.as_str(),
        ];
        let mut vus: HashMap<std::path::PathBuf, String> = HashMap::new();
        let mut collisions = Vec::new();

        // Les originaux occupent déjà le répertoire racine du cache.
        for h in &condensats {
            for ext in tune_core::library::artwork::CACHE_EXTENSIONS {
                vus.insert(
                    cache.join(format!("{h}.{ext}")),
                    format!("original {h}.{ext}"),
                );
            }
        }
        for h in &condensats {
            for &case in tune_core::library::artwork::THUMB_SIZES {
                let chemin = tune_core::library::artwork::thumb_path(cache, case, h);
                let qui = format!("vignette {h} case {case}");
                if let Some(autre) = vus.insert(chemin.clone(), qui.clone()) {
                    collisions.push(format!("{} == {} → {}", qui, autre, chemin.display()));
                }
            }
        }
        assert!(
            collisions.is_empty(),
            "{} collision(s) de clé de cache sur {} chemins (#1444) : {collisions:#?}",
            collisions.len(),
            vus.len()
        );
    }

    /// LA BORNE. Le client interpole la taille telle quelle dans l'URL, sans
    /// aucun contrôle. Une taille absurde ne doit ni allouer, ni échouer :
    /// elle retombe sur l'original, exactement comme aujourd'hui.
    #[tokio::test]
    async fn une_taille_hors_bornes_ne_fait_pas_allouer() {
        let cache = tempfile::TempDir::new().unwrap();
        let hash = "0000000000000000000000000000aa03";
        let origine = pochette(300);
        poser(cache.path(), hash, &origine);

        let mut echecs = Vec::new();
        for demande in [100_000u32, 65_536, 4096, 401, u32::MAX] {
            assert!(
                tune_core::library::artwork::thumb_bucket(demande).is_none(),
                "{demande} devrait être hors de toute case"
            );
            let reponse = serve_artwork_from(cache.path(), hash, Some(demande)).await;
            if reponse.status() != StatusCode::OK {
                echecs.push(format!("?size={demande} → {}", reponse.status()));
                continue;
            }
            let servi = corps(reponse).await;
            if servi != origine {
                echecs.push(format!(
                    "?size={demande} → {} o servis au lieu des {} o d'origine",
                    servi.len(),
                    origine.len()
                ));
            }
        }
        assert!(
            echecs.is_empty(),
            "une taille hors bornes doit dégrader vers l'original : {echecs:#?}"
        );
    }

    /// Une taille illisible (`?size=abc`, `?size=-1`) rendait l'image avant le
    /// correctif ; elle doit continuer à la rendre, pas devenir un 400.
    #[tokio::test]
    async fn une_taille_illisible_rend_toujours_l_image() {
        let cache = tempfile::TempDir::new().unwrap();
        let hash = "0000000000000000000000000000aa04";
        let origine = pochette(300);
        poser(cache.path(), hash, &origine);
        for brut in ["abc", "-1", "", "2.5", "1e9"] {
            let taille = brut.parse::<u32>().ok();
            let reponse = serve_artwork_from(cache.path(), hash, taille).await;
            assert_eq!(
                reponse.status(),
                StatusCode::OK,
                "?size={brut} ne doit pas transformer une pochette en erreur"
            );
            assert_eq!(corps(reponse).await, origine, "?size={brut}");
        }
    }

    /// JAMAIS D'AGRANDISSEMENT : une pochette déjà plus petite que la case est
    /// servie telle quelle. La ré-encoder en 200 px serait plus lourd que
    /// l'original — l'inverse du but.
    #[tokio::test]
    async fn une_pochette_plus_petite_que_la_case_est_servie_telle_quelle() {
        let cache = tempfile::TempDir::new().unwrap();
        let hash = "0000000000000000000000000000aa05";
        let origine = pochette(64);
        poser(cache.path(), hash, &origine);
        let reponse = serve_artwork_from(cache.path(), hash, Some(200)).await;
        assert_eq!(
            reponse.headers().get("ETag").unwrap().to_str().unwrap(),
            format!("\"{hash}\""),
            "une image non redimensionnée doit garder l'ETag de l'original"
        );
        assert_eq!(corps(reponse).await, origine);
    }

    /// L'ETag d'une vignette porte sa case. Deux tailles servies sous le même
    /// ETag avec `Cache-Control: immutable`, c'est un cache qui rend la
    /// vignette de 80 px là où la grille demande 200.
    #[tokio::test]
    async fn deux_cases_ne_partagent_pas_un_etag() {
        let cache = tempfile::TempDir::new().unwrap();
        let hash = "0000000000000000000000000000aa06";
        poser(cache.path(), hash, &pochette(600));
        let mut etags = std::collections::HashSet::new();
        let mut corps_vus = std::collections::HashSet::new();
        for case in [80u32, 128, 200, 400] {
            let reponse = serve_artwork_from(cache.path(), hash, Some(case)).await;
            let etag = reponse
                .headers()
                .get("ETag")
                .unwrap()
                .to_str()
                .unwrap()
                .to_string();
            assert_eq!(etag, format!("\"{hash}-w{case}\""));
            etags.insert(etag);
            corps_vus.insert(corps(reponse).await);
        }
        assert_eq!(etags.len(), 4, "quatre cases, quatre ETags distincts");
        assert_eq!(corps_vus.len(), 4, "quatre cases, quatre corps distincts");
    }

    /// La vignette est bien écrite sur disque, sous sa case, et relue au second
    /// appel : c'est ce qui rend le coût de calcul non récurrent.
    #[tokio::test]
    async fn la_vignette_est_mise_en_cache_sous_sa_case() {
        let cache = tempfile::TempDir::new().unwrap();
        let hash = "0000000000000000000000000000aa07";
        poser(cache.path(), hash, &pochette(600));

        let premier = corps(serve_artwork_from(cache.path(), hash, Some(200)).await).await;
        let sur_disque = tune_core::library::artwork::thumb_path(cache.path(), 200, hash);
        assert!(
            sur_disque.exists(),
            "vignette absente de {}",
            sur_disque.display()
        );
        assert_eq!(std::fs::read(&sur_disque).unwrap(), premier);

        let second = corps(serve_artwork_from(cache.path(), hash, Some(200)).await).await;
        assert_eq!(second, premier, "le second appel doit relire le cache");

        // Aucun fichier temporaire ne doit survivre au `rename`.
        let restes: Vec<_> =
            std::fs::read_dir(tune_core::library::artwork::thumb_dir(cache.path(), 200))
                .unwrap()
                .flatten()
                .map(|e| e.file_name().to_string_lossy().into_owned())
                .filter(|n| n.ends_with(".tmp"))
                .collect();
        assert!(
            restes.is_empty(),
            "fichiers temporaires laissés : {restes:?}"
        );
    }

    /// Un format que `image` ne sait pas décoder (WebP, BMP — présents dans
    /// `CACHE_EXTENSIONS` mais hors des features compilées) doit dégrader vers
    /// l'original, pas rendre une erreur.
    #[tokio::test]
    async fn un_format_non_decodable_degrade_vers_l_original() {
        let cache = tempfile::TempDir::new().unwrap();
        let hash = "0000000000000000000000000000aa08";
        let octets = b"RIFF....WEBPpas-une-image";
        std::fs::write(cache.path().join(format!("{hash}.webp")), octets).unwrap();
        let reponse = serve_artwork_from(cache.path(), hash, Some(80)).await;
        assert_eq!(reponse.status(), StatusCode::OK);
        assert_eq!(reponse.headers().get("Content-Type").unwrap(), "image/webp");
        assert_eq!(corps(reponse).await, octets);
    }

    /// LE JOURNAL. `artwork_cache_miss` émettait un `warn!` par requête : une
    /// grille de 50 pochettes manquantes en réécrivait 50 à chaque rendu, et un
    /// 404 n'est pas mis en cache par le navigateur. Le premier constat par
    /// condensat reste un `warn!`, les suivants passent en `debug!` ; et le jeu
    /// qui s'en souvient est borné.
    ///
    /// Les deux moitiés tiennent dans **un seul** test parce qu'elles écrivent
    /// dans le même état global : séparées, la moitié « borné » remplirait le
    /// jeu et ferait échouer l'autre selon l'ordre d'exécution.
    #[test]
    fn le_journal_d_absence_ne_suit_plus_le_trafic_d_interface() {
        let cache = std::path::Path::new("/cache");
        let hash = "0000000000000000000000000000aa09";

        assert!(
            premier_constat(cache, hash),
            "le tout premier constat doit être signalé"
        );
        let mut repetitions_signalees = 0;
        for _ in 0..49 {
            if premier_constat(cache, hash) {
                repetitions_signalees += 1;
            }
        }
        assert_eq!(
            repetitions_signalees, 0,
            "{repetitions_signalees} répétition(s) sur 49 encore signalées : le journal suit toujours le trafic d'interface (#2996)"
        );
        assert!(
            premier_constat(cache, "0000000000000000000000000000aa0a"),
            "un autre condensat garde droit à son premier constat"
        );

        // Bornage : le jeu ne peut pas grandir avec la bibliothèque.
        for i in 0..(PLAFOND_ABSENCES + 500) {
            journaliser_absence(cache, &format!("{i:064x}"));
        }
        let taille = ABSENCES_DEJA_SIGNALEES
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .len();
        assert!(
            taille <= PLAFOND_ABSENCES,
            "{taille} condensats retenus, plafond {PLAFOND_ABSENCES}"
        );
    }

    /// Miroir de la décision prise dans [`journaliser_absence`], pour la rendre
    /// observable sans lecteur de journal.
    fn premier_constat(cache_dir: &std::path::Path, hash: &str) -> bool {
        let avant = ABSENCES_DEJA_SIGNALEES
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .len();
        journaliser_absence(cache_dir, hash);
        let apres = ABSENCES_DEJA_SIGNALEES
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .len();
        apres > avant
    }
}

#[cfg(test)]
mod tests_avancement_pochettes {
    use super::*;
    use std::sync::Arc;
    use tune_core::event_bus::EventBus;

    /// Contrat INTER-DEPOTS (#2870). `SettingsView.svelte` declare
    /// `artworkProgress: { current: number; total: number; found: number }` et
    /// rend `settings.coversProgress` — « Covers {current}/{total} ({found}
    /// trouvées) ». Renommer un de ces trois champs affiche « undefined ».
    #[test]
    fn la_charge_porte_exactement_current_total_found() {
        let a = AvancementPochettes {
            traites: 12,
            total: 400,
            trouvees: 7,
        };
        let charge = a.charge();
        assert_eq!(charge["current"], 12);
        assert_eq!(charge["total"], 400);
        assert_eq!(charge["found"], 7);
        assert_eq!(
            charge.as_object().map(|o| o.len()),
            Some(3),
            "pas un champ de plus : le client n'en lit que trois"
        );
    }

    /// `library.artwork.completed` est le SEUL evenement qui fasse retomber
    /// `artworkScanning`. Il doit donc partir meme quand la passe est
    /// interrompue — sinon le bouton reste grise jusqu'au rechargement de la
    /// page, ce qui est exactement le defaut d'origine.
    #[tokio::test]
    async fn la_fin_est_annoncee_meme_si_la_passe_est_interrompue() {
        let bus = Arc::new(EventBus::new());
        let mut evenements = bus.subscribe();
        {
            let mut fin = FinDeReprise {
                bus: bus.clone(),
                avancement: AvancementPochettes {
                    traites: 0,
                    total: 900,
                    trouvees: 0,
                },
            };
            fin.avancement.traites = 3;
            fin.avancement.trouvees = 1;
            // On sort du bloc sans atteindre la fin de la boucle : c'est le
            // scenario « la tache s'arrete en cours de route ».
        }
        let ev = evenements.recv().await.expect("fin de reprise attendue");
        assert_eq!(ev.event_type, "library.artwork.completed");
        assert_eq!(ev.data["current"], 3, "le compte partiel, pas un mensonge");
        assert_eq!(ev.data["total"], 900);
        assert_eq!(ev.data["found"], 1);
    }

    /// Une bibliotheque VIDE doit quand meme annoncer la fin : sinon le bouton
    /// reste grise pour toujours chez qui n'a pas encore scanne.
    #[tokio::test]
    async fn une_bibliotheque_vide_annonce_quand_meme_la_fin() {
        let bus = Arc::new(EventBus::new());
        let mut evenements = bus.subscribe();
        drop(FinDeReprise {
            bus: bus.clone(),
            avancement: AvancementPochettes::default(),
        });
        let ev = evenements.recv().await.expect("fin de reprise attendue");
        assert_eq!(ev.event_type, "library.artwork.completed");
        assert_eq!(ev.data["total"], 0);
    }

    /// Contre-epreuve de la cadence : sur mille albums, l'avancement ne doit PAS
    /// produire mille evenements. Une emission par album noierait le bus et
    /// ferait prendre du retard aux clients (`Lagged`) — c'est le defaut que la
    /// discipline de `library.scan.progress` evite depuis toujours.
    #[test]
    fn mille_albums_ne_font_pas_mille_annonces() {
        use std::time::{Duration, Instant};
        let t0 = Instant::now();
        let mut cadence = tune_core::cadence::Cadence::avancement();
        let mut annonces = 0usize;
        for i in 0..1000u64 {
            // Un album toutes les 10 ms : 10 s de passe.
            if cadence.autorise_a(t0 + Duration::from_millis(i * 10)) {
                annonces += 1;
            }
        }
        assert_eq!(
            annonces, 5,
            "1000 albums a 10 ms couvrent 0..9,99 s : annonces a 0, 2, 4, 6 et 8 s"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Chaque passe doit s'annoncer sous son propre nom. La passe par nom
    /// n'appelle pas MusicBrainz : l'afficher ainsi désigne le mauvais service
    /// quand la recherche échoue (#2227, #2257).
    #[test]
    fn chaque_passe_porte_son_propre_libelle() {
        assert_eq!(libelle_phase(Some("mbid")), "MusicBrainz");
        assert_eq!(libelle_phase(Some("images")), "Images");
        assert_eq!(
            libelle_phase(Some(tune_core::library::artwork::PHASE_PAR_NOM)),
            "Discogs / Last.fm"
        );
        assert_ne!(
            libelle_phase(Some(tune_core::library::artwork::PHASE_PAR_NOM)),
            "MusicBrainz",
            "la passe par nom n'interroge pas MusicBrainz"
        );
        // Une passe inconnue garde le repli historique.
        assert_eq!(libelle_phase(None), "MusicBrainz");
    }
}
